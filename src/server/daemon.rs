use super::*;
use crate::server::protocol::{
    self, CreateInstanceRequest, ErrorCode, HealthResult, InstanceResult, InstanceState,
    ListResult, ProtocolError, Request as ProtocolRequest, RequestEnvelope,
    Response as ProtocolResponse, ResponseEnvelope, StopInstanceResult,
};
use std::net::SocketAddr;
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::sync::Mutex as AsyncMutex;

const INSTANCE_STOP_GRACE: Duration = SHUTDOWN_GRACE;

/// Foreground supervisor state. The protocol module owns the wire types; this
/// module owns the richer runtime state required by B3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonPhase {
    Running,
    ShuttingDown,
}

struct SupervisorState {
    instances: HashMap<String, Arc<DaemonInstance>>,
    phase: DaemonPhase,
}

type SharedSupervisorState = Arc<AsyncMutex<SupervisorState>>;

impl SupervisorState {
    fn shared() -> SharedSupervisorState {
        Arc::new(AsyncMutex::new(Self {
            instances: HashMap::new(),
            phase: DaemonPhase::Running,
        }))
    }
}

struct DaemonInstance {
    name: String,
    requested_host: String,
    requested_port: u16,
    address: SocketAddr,
    state: AsyncMutex<InstanceState>,
    shutdown: watch::Sender<bool>,
    cancel: watch::Sender<bool>,
    listener_task: AsyncMutex<Option<tokio::task::JoinHandle<Result<()>>>>,
    stop_lock: AsyncMutex<()>,
    routes: RwLock<HashMap<String, Arc<crate::serve::RouteRuntime>>>,
}

impl DaemonInstance {
    fn result(&self, state: InstanceState) -> InstanceResult {
        InstanceResult {
            name: self.name.clone(),
            host: self.requested_host.clone(),
            port: self.address.port(),
            address: format!("http://{}", self.address),
            state,
        }
    }
}

// Transitional compatibility surface for the pre-B4 client; the production
// daemon path below does not construct or expose this HTTP state.
#[allow(dead_code)]
#[derive(Clone)]
pub(super) struct ServerState {
    pub(super) server_name: String,
    pub(super) agents: Arc<RwLock<HashMap<String, RegisteredAgent>>>,
    pub(super) shutdown: watch::Sender<bool>,
    /// Cancels every registered agent once the shutdown drain grace expired,
    /// so their connection guards terminate the agent process groups.
    pub(super) cancel: watch::Sender<bool>,
}

#[allow(dead_code)]
#[derive(Clone)]
pub(super) struct RegisteredAgent {
    pub(super) id: String,
    pub(super) route: String,
    pub(super) router: Router,
    pub(super) readyz_endpoint: bool,
}

impl RegisteredAgent {
    #[cfg(test)]
    pub(super) fn new(id: String, route: String, router: Router) -> Self {
        Self {
            id,
            route,
            router,
            readyz_endpoint: true,
        }
    }
}

/// Runs the foreground Unix-socket supervisor.
///
/// The legacy arguments remain in the private entry point until the CLI client
/// is migrated in the next refactor node. They no longer create a per-name HTTP
/// daemon; one supervisor owns all instances in memory.
pub async fn run(_name: String, _host: String, _port: u16) -> Result<()> {
    run_supervisor().await
}

#[cfg(unix)]
async fn run_supervisor() -> Result<()> {
    let (listener, socket_path) = protocol::bind_daemon_socket()?;
    let state = SupervisorState::shared();
    let (shutdown, mut shutdown_rx) = watch::channel(false);
    tokio::spawn(crate::serve::await_termination_signal(shutdown.clone()));

    let result = loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("failed to accept daemon connection")?;
                let state = state.clone();
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_protocol_connection(stream, state, shutdown).await {
                        eprintln!("daemon request failed: {error:#}");
                    }
                });
            }
            () = crate::serve::wait_for_shutdown(&mut shutdown_rx) => {
                state.lock().await.phase = DaemonPhase::ShuttingDown;
                break Ok(());
            }
        }
    };

    state.lock().await.phase = DaemonPhase::ShuttingDown;
    stop_all_instances(&state).await;
    match tokio::fs::remove_file(&socket_path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to remove daemon socket {}", socket_path.display())
            });
        }
    }
    result
}

#[cfg(not(unix))]
async fn run_supervisor() -> Result<()> {
    anyhow::bail!("the named-server daemon requires Unix-domain sockets")
}

#[cfg(unix)]
async fn handle_protocol_connection(
    mut stream: UnixStream,
    state: SharedSupervisorState,
    shutdown: watch::Sender<bool>,
) -> Result<()> {
    let request = match protocol::read_frame::<_, RequestEnvelope>(&mut stream).await {
        Ok(request) => request,
        Err(error) => {
            let response = error_response(ErrorCode::InvalidInput, error.to_string());
            let _ = protocol::write_frame(&mut stream, &response).await;
            return Err(error);
        }
    };
    let shutdown_requested = request.version == protocol::PROTOCOL_VERSION
        && matches!(&request.command, ProtocolRequest::Shutdown);
    let response = if request.version != protocol::PROTOCOL_VERSION {
        error_response(
            ErrorCode::InvalidInput,
            format!(
                "unsupported protocol version {}; expected {}",
                request.version,
                protocol::PROTOCOL_VERSION
            ),
        )
    } else {
        handle_request(request.command, state).await
    };
    // The shutdown phase is entered while handling the request so concurrent
    // mutations are rejected, but the supervisor is not signaled until this
    // response has been flushed to the client.
    let result = protocol::write_frame(&mut stream, &response).await;
    if shutdown_requested && result.is_ok() {
        shutdown.send_replace(true);
    }
    result
}

#[cfg(unix)]
async fn handle_request(
    command: ProtocolRequest,
    state: SharedSupervisorState,
) -> ResponseEnvelope {
    if matches!(
        command,
        ProtocolRequest::CreateInstance(_)
            | ProtocolRequest::StopInstance(_)
            | ProtocolRequest::Register(_)
            | ProtocolRequest::Unregister(_)
    ) && state.lock().await.phase != DaemonPhase::Running
    {
        return error_response(ErrorCode::Operation, "daemon is shutting down");
    }
    match command {
        ProtocolRequest::Health => success(ProtocolResponse::Health(HealthResult {
            protocol_version: protocol::PROTOCOL_VERSION,
        })),
        ProtocolRequest::CreateInstance(request) => match create_instance(&state, request).await {
            Ok(instance) => success(ProtocolResponse::CreateInstance(instance)),
            Err(error) => error_response(error.code, error.message),
        },
        ProtocolRequest::StopInstance(request) => {
            match stop_instance(&state, &request.name).await {
                Ok(()) => success(ProtocolResponse::StopInstance(StopInstanceResult {
                    name: request.name,
                })),
                Err(error) => error_response(error.code, error.message),
            }
        }
        ProtocolRequest::List => success(ProtocolResponse::List(ListResult {
            instances: list_instances(&state).await,
        })),
        ProtocolRequest::Status(request) => match status_instance(&state, &request.name).await {
            Ok(instance) => success(ProtocolResponse::Status(instance)),
            Err(error) => error_response(error.code, error.message),
        },
        ProtocolRequest::Shutdown => {
            state.lock().await.phase = DaemonPhase::ShuttingDown;
            success(ProtocolResponse::Shutdown)
        }
        ProtocolRequest::Register(_) => unsupported("route registration is not implemented yet"),
        ProtocolRequest::Unregister(_) => {
            unsupported("route unregistration is not implemented yet")
        }
        ProtocolRequest::Registrations(_) => unsupported("route inspection is not implemented yet"),
    }
}

fn success(result: ProtocolResponse) -> ResponseEnvelope {
    ResponseEnvelope::Success {
        version: protocol::PROTOCOL_VERSION,
        result,
    }
}

fn error_response(code: ErrorCode, message: impl Into<String>) -> ResponseEnvelope {
    ResponseEnvelope::Error {
        version: protocol::PROTOCOL_VERSION,
        error: ProtocolError {
            code,
            message: message.into(),
        },
    }
}

fn unsupported(message: &str) -> ResponseEnvelope {
    error_response(ErrorCode::Operation, message)
}

#[derive(Debug)]
struct DaemonOperationError {
    code: ErrorCode,
    message: String,
}

impl DaemonOperationError {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

async fn create_instance(
    state: &SharedSupervisorState,
    request: CreateInstanceRequest,
) -> std::result::Result<InstanceResult, DaemonOperationError> {
    validate_name(&request.name)
        .map_err(|error| DaemonOperationError::new(ErrorCode::InvalidInput, error.to_string()))?;
    if request.host.trim().is_empty() {
        return Err(DaemonOperationError::new(
            ErrorCode::InvalidInput,
            "instance host must not be empty",
        ));
    }

    // This lock serializes same-name creation. Binding occurs before insertion,
    // so every committed entry owns a live listener and no partial instance is
    // observable after a bind failure.
    let mut daemon = state.lock().await;
    if daemon.phase != DaemonPhase::Running {
        return Err(DaemonOperationError::new(
            ErrorCode::Operation,
            "daemon is shutting down",
        ));
    }
    if let Some(existing) = daemon.instances.get(&request.name) {
        let current = *existing.state.lock().await;
        if existing.requested_host == request.host && existing.requested_port == request.port {
            return Ok(existing.result(current));
        }
        return Err(DaemonOperationError::new(
            ErrorCode::Conflict,
            format!(
                "instance {:?} already exists with a different configuration",
                request.name
            ),
        ));
    }

    let listener = TcpListener::bind((request.host.as_str(), request.port))
        .await
        .map_err(|error| {
            DaemonOperationError::new(
                ErrorCode::Unavailable,
                format!(
                    "failed to bind instance {}:{}: {error}",
                    request.host, request.port
                ),
            )
        })?;
    let address = listener.local_addr().map_err(|error| {
        DaemonOperationError::new(
            ErrorCode::Operation,
            format!("failed to read instance listener address: {error}"),
        )
    })?;
    let (shutdown, shutdown_rx) = watch::channel(false);
    let (cancel, _) = watch::channel(false);
    let instance = Arc::new(DaemonInstance {
        name: request.name.clone(),
        requested_host: request.host,
        requested_port: request.port,
        address,
        state: AsyncMutex::new(InstanceState::Running),
        shutdown,
        cancel: cancel.clone(),
        listener_task: AsyncMutex::new(None),
        stop_lock: AsyncMutex::new(()),
        routes: RwLock::new(HashMap::new()),
    });
    let router = Router::new()
        .fallback(dispatch_instance)
        .with_state(instance.clone());
    let task_cancel = instance.cancel.clone();
    let task = tokio::spawn(async move {
        crate::serve::serve_with_shutdown(
            listener,
            router,
            shutdown_rx,
            task_cancel,
            INSTANCE_STOP_GRACE,
        )
        .await
    });
    *instance.listener_task.lock().await = Some(task);
    daemon.instances.insert(request.name, instance.clone());
    Ok(instance.result(InstanceState::Running))
}

async fn list_instances(state: &SharedSupervisorState) -> Vec<InstanceResult> {
    let instances = {
        let daemon = state.lock().await;
        daemon.instances.values().cloned().collect::<Vec<_>>()
    };
    let mut result = Vec::with_capacity(instances.len());
    for instance in instances {
        let current = *instance.state.lock().await;
        result.push(instance.result(current));
    }
    result.sort_by(|left, right| left.name.cmp(&right.name));
    result
}

async fn status_instance(
    state: &SharedSupervisorState,
    name: &str,
) -> std::result::Result<InstanceResult, DaemonOperationError> {
    let instance = {
        let daemon = state.lock().await;
        daemon.instances.get(name).cloned()
    };
    let Some(instance) = instance else {
        return Err(DaemonOperationError::new(
            ErrorCode::NotFound,
            format!("instance {name:?} was not found"),
        ));
    };
    let current = *instance.state.lock().await;
    Ok(instance.result(current))
}

async fn stop_instance(
    state: &SharedSupervisorState,
    name: &str,
) -> std::result::Result<(), DaemonOperationError> {
    let instance = {
        let daemon = state.lock().await;
        daemon.instances.get(name).cloned()
    };
    let Some(instance) = instance else {
        return Err(DaemonOperationError::new(
            ErrorCode::NotFound,
            format!("instance {name:?} was not found"),
        ));
    };
    let _stop_guard = instance.stop_lock.lock().await;
    {
        let mut current = instance.state.lock().await;
        if *current == InstanceState::Running {
            *current = InstanceState::Stopping;
            instance.shutdown.send_replace(true);
        }
    }

    // `serve_with_shutdown` owns the bounded graceful-drain and force-close
    // path. Taking the handle and awaiting it keeps completion explicit and
    // prevents a stop timeout from implicitly detaching the listener task.
    let task = instance.listener_task.lock().await.take();
    let task_result = match task {
        Some(task) => match task.await {
            Ok(result) => result,
            Err(error) => Err(anyhow::anyhow!("instance listener task failed: {error}")),
        },
        None => Ok(()),
    };
    let mut daemon = state.lock().await;
    if daemon
        .instances
        .get(name)
        .is_some_and(|candidate| Arc::ptr_eq(candidate, &instance))
    {
        daemon.instances.remove(name);
    }
    task_result.map_err(|error| {
        DaemonOperationError::new(
            ErrorCode::Operation,
            format!("instance listener failed: {error:#}"),
        )
    })
}

async fn stop_all_instances(state: &SharedSupervisorState) {
    let names = {
        let daemon = state.lock().await;
        daemon.instances.keys().cloned().collect::<Vec<_>>()
    };
    for name in names {
        let _ = stop_instance(state, &name).await;
    }
}

async fn dispatch_instance(
    State(instance): State<Arc<DaemonInstance>>,
    request: Request<Body>,
) -> Response {
    let path = request.uri().path();
    let route = {
        let routes = instance.routes.read().await;
        routes
            .iter()
            .filter(|(route, _)| route_matches(route, path))
            .max_by_key(|(route, _)| route.len())
            .map(|(route, runtime)| (route.clone(), runtime.clone()))
    };
    let Some((route, runtime)) = route else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let request = match rewrite_route_prefix(request, &route) {
        Ok(request) => request,
        Err(error) => return StatusCode::INTERNAL_SERVER_ERROR.into_response_with_body(error),
    };
    // Keep readiness ownership with the route runtime. B5 will expose this
    // snapshot through the control protocol; dispatch must retain the same
    // runtime Arc while the request is in flight.
    let _readiness = runtime.readiness_snapshot();
    runtime.router().oneshot(request).await.into_response()
}

trait ResponseErrorExt {
    fn into_response_with_body(self, detail: String) -> Response;
}

impl ResponseErrorExt for StatusCode {
    fn into_response_with_body(self, detail: String) -> Response {
        (self, detail).into_response()
    }
}

#[allow(dead_code)]
pub(super) async fn run_with(
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
    // SIGINT/SIGTERM stop the daemon exactly like the control-plane shutdown
    // endpoint: gracefully drain connections within `shutdown_grace` before
    // cancelling agents so their process groups are terminated.
    tokio::spawn(crate::serve::await_termination_signal(shutdown_tx.clone()));
    let (cancel, _) = watch::channel(false);
    let state = ServerState {
        server_name: name.clone(),
        agents: Arc::default(),
        shutdown: shutdown_tx,
        cancel: cancel.clone(),
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
    write_private_json_exclusive(&state_path, &file, &name)?;
    eprintln!(
        "Serving named ACP server \"{name}\" at {}",
        public_url(&file.listen_host, file.port)?
    );

    let result =
        crate::serve::serve_with_shutdown(listener, router, shutdown_rx, cancel, shutdown_grace)
            .await;
    let cleanup = match tokio::fs::remove_file(&state_path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to remove server state {}", state_path.display())),
    };
    result.and(cleanup)
}

// Test-only wrapper preserving the daemon's internal 4-argument shutdown API;
// production `run_with` wires the real cancellation sender.
#[cfg(test)]
pub(super) async fn serve_with_shutdown(
    listener: TcpListener,
    router: Router,
    shutdown_rx: watch::Receiver<bool>,
    shutdown_grace: Duration,
) -> Result<()> {
    crate::serve::serve_with_shutdown(
        listener,
        router,
        shutdown_rx,
        watch::channel(false).0,
        shutdown_grace,
    )
    .await
}

#[allow(dead_code)]
pub(super) fn server_router(state: ServerState) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/status", get(server_status))
        .route("/api/registrations", get(server_registrations))
        .route("/api/agents", post(add_agent).delete(remove_agent))
        .route("/api/shutdown", post(shutdown))
        .fallback(dispatch_agent)
        .with_state(state)
}

#[allow(dead_code)]
pub(super) async fn server_status(State(state): State<ServerState>) -> Json<ServerStatus> {
    Json(ServerStatus {
        name: state.server_name,
        pid: std::process::id(),
        version: SERVER_PROTOCOL_VERSION.to_string(),
    })
}

// Readiness is probed by the CLI, keeping this control endpoint cheap.
#[allow(dead_code)]
pub(super) async fn server_registrations(
    State(state): State<ServerState>,
) -> Json<Vec<RegistrationInfo>> {
    let mut registrations: Vec<_> = {
        let agents = state.agents.read().await;
        agents
            .values()
            .map(|agent| RegistrationInfo {
                id: agent.id.clone(),
                route: agent.route.clone(),
                readyz_endpoint: agent.readyz_endpoint,
            })
            .collect()
    };
    registrations.sort_by(|left, right| {
        left.route
            .cmp(&right.route)
            .then_with(|| left.id.cmp(&right.id))
    });
    Json(registrations)
}

#[allow(dead_code)]
pub(super) async fn add_agent(
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
    let resolved = match crate::runner::resolve_agent_config_from_registry_agent(agent, &args).await
    {
        Ok(resolved) => resolved,
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
    let router = match crate::serve::agent_router_with_lease(
        resolved.config,
        &options,
        state.cancel.subscribe(),
        resolved.cache_use_lease,
    ) {
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
        readyz_endpoint: registration.serve.readyz_endpoint,
    };

    match insert_agent(&state, entry).await {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(error) => error,
    }
}

pub(super) fn api_error_detail(body: &str) -> String {
    match serde_json::from_str::<ApiError>(body) {
        Ok(error) => format!("{}: {}", error.error, error.message),
        Err(_) if body.trim().is_empty() => "server returned no error details".to_string(),
        Err(_) => body.to_string(),
    }
}

#[allow(dead_code)]
pub(super) fn serve_options(
    request: &AgentServeRequest,
) -> Result<crate::serve::AgentRouterOptions> {
    let options = crate::serve::AgentRouterOptions {
        path: request.path.clone(),
        cors: crate::serve::cors_options(request.cors_origins.clone(), request.allow_any_origin)?,
        health_endpoint: request.health_endpoint,
        readyz_endpoint: request.readyz_endpoint,
        max_processes: request.max_processes,
    };
    crate::serve::validate_router_options(&options)?;
    Ok(options)
}

#[allow(dead_code)]
pub(super) async fn resolved_args(id: &str, request: &AgentServeRequest) -> Result<Vec<String>> {
    crate::yolo::resolve_args(id, request.yolo, request.args.clone()).await
}

#[allow(dead_code)]
pub(super) async fn insert_agent(
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

#[allow(dead_code)]
pub(super) async fn remove_agent(
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

#[allow(dead_code)]
pub(super) async fn shutdown(State(state): State<ServerState>) -> Response {
    state.shutdown.send_replace(true);
    StatusCode::ACCEPTED.into_response()
}

#[allow(dead_code)]
pub(super) async fn dispatch_agent(
    State(state): State<ServerState>,
    request: Request<Body>,
) -> Response {
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

pub(super) fn rewrite_route_prefix(
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

pub(super) async fn load_live_server(name: &str) -> Result<ServerFile> {
    let state: ServerFile = read_json(&ServerPaths::discover()?.state_file(name))
        .await
        .with_context(|| format!("server \"{name}\" is not running"))?;
    if !server_is_alive(&state).await {
        bail!("server \"{name}\" is not running");
    }
    Ok(state)
}

pub(super) async fn server_is_alive(state: &ServerFile) -> bool {
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
    response
        .json::<ServerStatus>()
        .await
        .is_ok_and(|status| status.name == state.name && status.version == state.version)
}

pub(super) fn route_matches(route: &str, path: &str) -> bool {
    path == route
        || path
            .strip_prefix(route)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

pub(super) fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("server name must contain only letters, digits, '.', '-' or '_'");
    }
    Ok(())
}

pub(super) fn validate_route(route: &str) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn create_request(name: &str, host: &str, port: u16) -> CreateInstanceRequest {
        CreateInstanceRequest {
            name: name.to_string(),
            host: host.to_string(),
            port,
        }
    }

    #[cfg(unix)]
    async fn stop_test_instance(state: &SharedSupervisorState, name: &str) {
        stop_instance(state, name).await.unwrap();
        assert!(!state.lock().await.instances.contains_key(name));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_is_bind_before_commit_and_port_zero_reports_actual_address() {
        let state = SupervisorState::shared();
        let result = create_instance(&state, create_request("ephemeral", "127.0.0.1", 0))
            .await
            .unwrap();

        assert_eq!(result.name, "ephemeral");
        assert_eq!(result.host, "127.0.0.1");
        assert_ne!(result.port, 0);
        assert_eq!(result.address, format!("http://127.0.0.1:{}", result.port));
        assert_eq!(state.lock().await.instances.len(), 1);
        stop_test_instance(&state, "ephemeral").await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_equal_creates_are_idempotent_and_conflicts_do_not_bind() {
        let state = SupervisorState::shared();
        let request = create_request("shared", "127.0.0.1", 0);
        let (first, second) = tokio::join!(
            create_instance(&state, request.clone()),
            create_instance(&state, request),
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(first, second);

        let conflict = create_instance(&state, create_request("shared", "127.0.0.1", 1))
            .await
            .unwrap_err();
        assert_eq!(conflict.code, ErrorCode::Conflict);
        assert_eq!(state.lock().await.instances.len(), 1);
        stop_test_instance(&state, "shared").await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_bind_leaves_no_instance() {
        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = occupied.local_addr().unwrap().port();
        let state = SupervisorState::shared();
        let error = create_instance(&state, create_request("occupied", "127.0.0.1", port))
            .await
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::Unavailable);
        assert!(state.lock().await.instances.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_marks_and_removes_instance_and_public_listener_has_no_management_routes() {
        let state = SupervisorState::shared();
        let result = create_instance(&state, create_request("public", "127.0.0.1", 0))
            .await
            .unwrap();
        let address = result.address.clone();
        let response = reqwest::Client::new()
            .get(format!("{address}/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        stop_test_instance(&state, "public").await;
        assert!(matches!(
            status_instance(&state, "public").await,
            Err(DaemonOperationError {
                code: ErrorCode::NotFound,
                ..
            })
        ));
        assert!(
            reqwest::Client::new()
                .get(format!("{address}/health"))
                .send()
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn protocol_handles_health_list_and_explicitly_rejects_registration() {
        let state = SupervisorState::shared();
        let health = handle_request(ProtocolRequest::Health, state.clone()).await;
        assert!(matches!(
            health,
            ResponseEnvelope::Success {
                result: ProtocolResponse::Health(_),
                ..
            }
        ));
        let list = handle_request(ProtocolRequest::List, state.clone()).await;
        assert!(matches!(
            list,
            ResponseEnvelope::Success {
                result: ProtocolResponse::List(_),
                ..
            }
        ));
        let registration = handle_request(
            ProtocolRequest::Register(crate::server::protocol::RegisterRequest {
                name: "default".into(),
                id: "agent".into(),
                route: "/agent".into(),
                path: "/acp".into(),
                cors_origins: Vec::new(),
                allow_any_origin: false,
                health_endpoint: true,
                readyz_endpoint: true,
                max_processes: 1,
                yolo: false,
                args: Vec::new(),
            }),
            state,
        )
        .await;
        assert!(matches!(
            registration,
            ResponseEnvelope::Error {
                error: ProtocolError {
                    code: ErrorCode::Operation,
                    ..
                },
                ..
            }
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_connection_round_trips_typed_protocol_frames() {
        let state = SupervisorState::shared();
        let (shutdown, _) = watch::channel(false);
        let (mut client, server) = UnixStream::pair().unwrap();
        let task = tokio::spawn(handle_protocol_connection(server, state.clone(), shutdown));
        protocol::write_frame(
            &mut client,
            &RequestEnvelope {
                version: protocol::PROTOCOL_VERSION,
                command: ProtocolRequest::Health,
            },
        )
        .await
        .unwrap();
        let response: ResponseEnvelope = protocol::read_frame(&mut client).await.unwrap();
        assert!(matches!(
            response,
            ResponseEnvelope::Success {
                result: ProtocolResponse::Health(HealthResult {
                    protocol_version: 1
                }),
                ..
            }
        ));
        task.await.unwrap().unwrap();

        let (mut client, server) = UnixStream::pair().unwrap();
        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(handle_protocol_connection(server, state.clone(), shutdown));
        protocol::write_frame(
            &mut client,
            &RequestEnvelope {
                version: protocol::PROTOCOL_VERSION,
                command: ProtocolRequest::Shutdown,
            },
        )
        .await
        .unwrap();
        let response: ResponseEnvelope = protocol::read_frame(&mut client).await.unwrap();
        assert!(matches!(
            response,
            ResponseEnvelope::Success {
                result: ProtocolResponse::Shutdown,
                ..
            }
        ));
        shutdown_rx.changed().await.unwrap();
        assert!(*shutdown_rx.borrow());
        task.await.unwrap().unwrap();
        assert_eq!(state.lock().await.phase, DaemonPhase::ShuttingDown);
        assert!(matches!(
            handle_request(
                ProtocolRequest::CreateInstance(create_request("late", "127.0.0.1", 0)),
                state,
            )
            .await,
            ResponseEnvelope::Error {
                error: ProtocolError {
                    code: ErrorCode::Operation,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn serve_options_preserve_requested_process_limit() {
        let options = serve_options(&crate::server::AgentServeRequest {
            path: "/rpc".to_string(),
            cors_origins: Vec::new(),
            allow_any_origin: false,
            health_endpoint: true,
            readyz_endpoint: true,
            max_processes: 3,
            yolo: false,
            args: Vec::new(),
        })
        .unwrap();

        assert_eq!(options.path, "/rpc");
        assert_eq!(options.max_processes, 3);
    }

    #[test]
    fn serve_options_reject_zero_process_limit() {
        let error = serve_options(&crate::server::AgentServeRequest {
            path: "/rpc".to_string(),
            cors_origins: Vec::new(),
            allow_any_origin: false,
            health_endpoint: true,
            readyz_endpoint: true,
            max_processes: 0,
            yolo: false,
            args: Vec::new(),
        })
        .unwrap_err();

        assert!(error.to_string().contains("max_processes"));
    }

    #[test]
    fn serve_options_rejects_process_limit_above_tokio_maximum() {
        let error = serve_options(&AgentServeRequest {
            path: "/acp".to_string(),
            cors_origins: Vec::new(),
            allow_any_origin: false,
            health_endpoint: true,
            readyz_endpoint: true,
            max_processes: tokio::sync::Semaphore::MAX_PERMITS.saturating_add(1),
            yolo: false,
            args: Vec::new(),
        })
        .unwrap_err();
        assert!(error.to_string().contains("must not exceed"));
    }
}
