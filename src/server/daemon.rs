use super::protocol::{
    self, CreateInstanceRequest, ErrorCode, HealthResult, InstanceResult, InstanceState,
    ListResult, ProtocolError, ReadinessResult, ReadinessStatus, RegisterRequest,
    RegistrationResult, RegistrationsResult, Request as ProtocolRequest, RequestEnvelope,
    Response as ProtocolResponse, ResponseEnvelope, StopInstanceResult, UnregisterResult,
};
use anyhow::{Context, Result, bail};
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use std::path::PathBuf;
use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};
use tokio::net::TcpListener;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex as AsyncMutex, RwLock, watch};
use tower::ServiceExt;

const INSTANCE_STOP_GRACE: Duration = super::SHUTDOWN_GRACE;

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

/// Route identity and the metadata needed by control-plane inspection.
///
/// Equality and hashing intentionally use only `(id, route)`: the remaining
/// fields describe the committed route and are returned to clients, while an
/// agent id or public route cannot be registered twice in one instance.
#[derive(Debug, Clone)]
struct RouteId {
    id: String,
    /// Public route prefix used to select this runtime.
    public_route: String,
    config: crate::serve::RouteConfig,
}

impl PartialEq for RouteId {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.public_route == other.public_route
    }
}

impl Eq for RouteId {}

impl Hash for RouteId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        self.public_route.hash(state);
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
    routes: RwLock<HashMap<RouteId, Arc<crate::serve::RouteRuntime>>>,
}

impl DaemonInstance {
    fn result(&self, state: InstanceState) -> InstanceResult {
        InstanceResult {
            name: self.name.clone(),
            host: self.requested_host.clone(),
            port: self.address.port(),
            address: public_address(self.address),
            state,
        }
    }
}

/// Runs the foreground Unix-socket supervisor.
pub async fn run() -> Result<()> {
    run_supervisor().await
}

async fn run_supervisor() -> Result<()> {
    let (listener, socket_path) = protocol::bind_daemon_socket()?;
    let state = SupervisorState::shared();
    let (shutdown, shutdown_rx) = watch::channel(false);
    tokio::spawn(crate::serve::await_termination_signal(shutdown.clone()));
    run_supervisor_loop(listener, socket_path, state, shutdown, shutdown_rx).await
}

async fn run_supervisor_loop(
    listener: UnixListener,
    socket_path: PathBuf,
    state: SharedSupervisorState,
    shutdown: watch::Sender<bool>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let result = loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(connection) => connection,
                    Err(error) => break Err(error).context("failed to accept daemon connection"),
                };
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
    let result = protocol::write_frame(&mut stream, &response).await;
    if shutdown_requested {
        shutdown.send_replace(true);
    }
    result
}

async fn handle_request(
    command: ProtocolRequest,
    state: SharedSupervisorState,
) -> ResponseEnvelope {
    let mutation = matches!(
        &command,
        ProtocolRequest::CreateInstance(_)
            | ProtocolRequest::StopInstance(_)
            | ProtocolRequest::Register(_)
            | ProtocolRequest::Unregister(_)
    );
    if mutation && state.lock().await.phase != DaemonPhase::Running {
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
        ProtocolRequest::Register(request) => match register(&state, request).await {
            Ok(result) => success(ProtocolResponse::Register(result)),
            Err(error) => error_response(error.code, error.message),
        },
        ProtocolRequest::Unregister(request) => match unregister(&state, request).await {
            Ok(result) => success(ProtocolResponse::Unregister(result)),
            Err(error) => error_response(error.code, error.message),
        },
        ProtocolRequest::List => success(ProtocolResponse::List(ListResult {
            instances: list_instances(&state).await,
        })),
        ProtocolRequest::Status(request) => match status_instance(&state, &request.name).await {
            Ok(instance) => success(ProtocolResponse::Status(instance)),
            Err(error) => error_response(error.code, error.message),
        },
        ProtocolRequest::Registrations(request) => {
            match registrations(&state, &request.name).await {
                Ok(result) => success(ProtocolResponse::Registrations(result)),
                Err(error) => error_response(error.code, error.message),
            }
        }
        ProtocolRequest::Shutdown => {
            state.lock().await.phase = DaemonPhase::ShuttingDown;
            success(ProtocolResponse::Shutdown)
        }
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

    let mut daemon = state.lock().await;
    if daemon.phase != DaemonPhase::Running {
        return Err(DaemonOperationError::new(
            ErrorCode::Operation,
            "daemon is shutting down",
        ));
    }
    if let Some(existing) = daemon.instances.get(&request.name) {
        let current = *existing.state.lock().await;
        if current != InstanceState::Running {
            return Err(DaemonOperationError::new(
                ErrorCode::Operation,
                format!("instance {:?} is stopping", request.name),
            ));
        }
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
    let task = tokio::spawn(async move {
        crate::serve::serve_with_shutdown(
            listener,
            router,
            shutdown_rx,
            cancel,
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
    let instance = instance_for(state, name).await?;
    let current = *instance.state.lock().await;
    Ok(instance.result(current))
}

async fn stop_instance(
    state: &SharedSupervisorState,
    name: &str,
) -> std::result::Result<(), DaemonOperationError> {
    let instance = instance_for(state, name).await?;
    let _stop_guard = instance.stop_lock.lock().await;
    {
        let mut current = instance.state.lock().await;
        if *current == InstanceState::Running {
            *current = InstanceState::Stopping;
            instance.shutdown.send_replace(true);
        }
    }

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

async fn instance_for(
    state: &SharedSupervisorState,
    name: &str,
) -> std::result::Result<Arc<DaemonInstance>, DaemonOperationError> {
    state
        .lock()
        .await
        .instances
        .get(name)
        .cloned()
        .ok_or_else(|| {
            DaemonOperationError::new(
                ErrorCode::NotFound,
                format!("instance {name:?} was not found"),
            )
        })
}

async fn register(
    state: &SharedSupervisorState,
    request: RegisterRequest,
) -> std::result::Result<RegistrationResult, DaemonOperationError> {
    validate_name(&request.name)
        .map_err(|error| DaemonOperationError::new(ErrorCode::InvalidInput, error.to_string()))?;
    validate_agent_id(&request.id)?;
    validate_route(&request.route)
        .map_err(|error| DaemonOperationError::new(ErrorCode::InvalidInput, error.to_string()))?;
    let instance = instance_for(state, &request.name).await?;
    ensure_running(&instance).await?;

    let options = route_config(&request)
        .map_err(|error| DaemonOperationError::new(ErrorCode::InvalidInput, error.to_string()))?;
    let registry = crate::registry::fetch_registry().await.map_err(|error| {
        DaemonOperationError::new(
            ErrorCode::Unavailable,
            format!("failed to fetch agent registry: {error:#}"),
        )
    })?;
    let agent = registry.find_agent(&request.id).ok_or_else(|| {
        DaemonOperationError::new(
            ErrorCode::NotFound,
            format!("agent {} was not found in the registry", request.id),
        )
    })?;
    let args = crate::yolo::resolve_args(&request.id, request.yolo, request.args.clone())
        .await
        .map_err(|error| DaemonOperationError::new(ErrorCode::InvalidInput, error.to_string()))?;
    let resolved = crate::runner::resolve_agent_config_from_registry_agent(agent, &args)
        .await
        .map_err(|error| {
            DaemonOperationError::new(
                ErrorCode::Unavailable,
                format!("failed to resolve agent {}: {error:#}", request.id),
            )
        })?;
    let runtime = crate::serve::RouteRuntime::new(resolved, &options, instance.cancel.subscribe())
        .map_err(|error| {
            DaemonOperationError::new(
                ErrorCode::InvalidInput,
                format!("failed to construct route runtime: {error:#}"),
            )
        })?;
    let route_id = RouteId {
        id: request.id,
        public_route: request.route,
        config: options,
    };
    let runtime = Arc::new(runtime);

    let _stop_guard = instance.stop_lock.lock().await;
    ensure_running(&instance).await?;
    let daemon = state.lock().await;
    if daemon.phase != DaemonPhase::Running {
        return Err(DaemonOperationError::new(
            ErrorCode::Operation,
            "daemon is shutting down",
        ));
    }
    let mut routes = instance.routes.write().await;
    if routes.keys().any(|existing| existing.id == route_id.id) {
        return Err(DaemonOperationError::new(
            ErrorCode::Conflict,
            format!("agent id {} is already registered", route_id.id),
        ));
    }
    if routes
        .keys()
        .any(|existing| existing.public_route == route_id.public_route)
    {
        return Err(DaemonOperationError::new(
            ErrorCode::Conflict,
            format!("route {} is already registered", route_id.public_route),
        ));
    }
    let result = registration_result(&instance.name, &instance.address, &route_id, &runtime);
    routes.insert(route_id, runtime);
    Ok(result)
}

async fn unregister(
    state: &SharedSupervisorState,
    request: super::protocol::UnregisterRequest,
) -> std::result::Result<UnregisterResult, DaemonOperationError> {
    validate_name(&request.name)
        .map_err(|error| DaemonOperationError::new(ErrorCode::InvalidInput, error.to_string()))?;
    validate_agent_id(&request.id)?;
    let instance = instance_for(state, &request.name).await?;
    let _stop_guard = instance.stop_lock.lock().await;
    ensure_running(&instance).await?;
    let daemon = state.lock().await;
    if daemon.phase != DaemonPhase::Running {
        return Err(DaemonOperationError::new(
            ErrorCode::Operation,
            "daemon is shutting down",
        ));
    }
    let mut routes = instance.routes.write().await;
    let route = routes
        .keys()
        .find(|route| route.id == request.id)
        .cloned()
        .ok_or_else(|| {
            DaemonOperationError::new(
                ErrorCode::NotFound,
                format!("agent {} is not registered", request.id),
            )
        })?;
    routes.remove(&route);
    Ok(UnregisterResult {
        name: request.name,
        id: request.id,
    })
}

async fn registrations(
    state: &SharedSupervisorState,
    name: &str,
) -> std::result::Result<RegistrationsResult, DaemonOperationError> {
    let instance = instance_for(state, name).await?;
    let routes = {
        let routes = instance.routes.read().await;
        routes
            .iter()
            .map(|(route, runtime)| (route.clone(), runtime.clone()))
            .collect::<Vec<_>>()
    };
    let mut registrations = routes
        .iter()
        .map(|(route, runtime)| registration_result(name, &instance.address, route, runtime))
        .collect::<Vec<_>>();
    registrations.sort_by(|left, right| {
        left.route
            .cmp(&right.route)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(RegistrationsResult {
        name: name.to_string(),
        registrations,
    })
}

fn registration_result(
    name: &str,
    address: &SocketAddr,
    route: &RouteId,
    runtime: &crate::serve::RouteRuntime,
) -> RegistrationResult {
    RegistrationResult {
        name: name.to_string(),
        id: route.id.clone(),
        route: route.public_route.clone(),
        path: route.config.path.clone(),
        health_endpoint: route.config.health_endpoint,
        readyz_endpoint: route.config.readyz_endpoint,
        address: public_address(*address),
        max_processes: route.config.max_processes,
        readiness: readiness_result(route.config.readyz_endpoint, runtime.readiness_snapshot()),
    }
}

fn public_address(address: SocketAddr) -> String {
    let host = match address.ip() {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => "127.0.0.1".to_string(),
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => "[::1]".to_string(),
        ip => match ip {
            std::net::IpAddr::V4(ip) => ip.to_string(),
            std::net::IpAddr::V6(ip) => format!("[{ip}]"),
        },
    };
    format!("http://{host}:{}", address.port())
}

fn readiness_result(
    readyz_endpoint: bool,
    snapshot: crate::serve::ReadinessSnapshot,
) -> ReadinessResult {
    let (status, detail) = if !readyz_endpoint {
        (ReadinessStatus::Disabled, None)
    } else if snapshot.last_attempt_failed {
        (
            ReadinessStatus::NotReady,
            snapshot.last_failure.map(|failure| failure.detail),
        )
    } else {
        (ReadinessStatus::Ready, None)
    };
    ReadinessResult {
        status,
        attempts: snapshot.attempts,
        failures: snapshot.failures,
        detail,
    }
}

async fn ensure_running(
    instance: &DaemonInstance,
) -> std::result::Result<(), DaemonOperationError> {
    if *instance.state.lock().await != InstanceState::Running {
        return Err(DaemonOperationError::new(
            ErrorCode::Operation,
            format!("instance {} is stopping", instance.name),
        ));
    }
    Ok(())
}

fn route_config(request: &RegisterRequest) -> Result<crate::serve::RouteConfig> {
    crate::serve::validate_route_config(&request.config)?;
    Ok(request.config.clone())
}

async fn dispatch_instance(
    State(instance): State<Arc<DaemonInstance>>,
    request: Request<Body>,
) -> Response {
    // Hold the state lock through route selection so stop cannot transition the
    // instance between the admission check and the runtime snapshot.
    let state = instance.state.lock().await;
    if *state != InstanceState::Running {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let path = request.uri().path();
    let route = {
        let routes = instance.routes.read().await;
        routes
            .iter()
            .filter(|(route, _)| route_matches(&route.public_route, path))
            .max_by_key(|(route, _)| route.public_route.len())
            .map(|(route, runtime)| (route.public_route.clone(), runtime.clone()))
    };
    drop(state);
    let Some((route, runtime)) = route else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let request = match rewrite_route_prefix(request, &route) {
        Ok(request) => request,
        Err(error) => return (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    };
    runtime.router().oneshot(request).await.into_response()
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

fn validate_agent_id(id: &str) -> std::result::Result<(), DaemonOperationError> {
    if id.trim().is_empty() {
        return Err(DaemonOperationError::new(
            ErrorCode::InvalidInput,
            "agent id must not be empty",
        ));
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header;
    use tempfile::TempDir;
    use tokio::net::{TcpStream, UnixListener};
    use tokio::time::timeout;

    fn create_request(name: &str, host: &str, port: u16) -> CreateInstanceRequest {
        CreateInstanceRequest {
            name: name.to_string(),
            host: host.to_string(),
            port,
        }
    }

    async fn stop_test_instance(state: &SharedSupervisorState, name: &str) {
        stop_instance(state, name).await.unwrap();
        assert!(!state.lock().await.instances.contains_key(name));
    }

    struct SupervisorHarness {
        socket_path: std::path::PathBuf,
        task: tokio::task::JoinHandle<Result<()>>,
        _tempdir: TempDir,
    }

    impl SupervisorHarness {
        async fn start() -> Self {
            let tempdir = tempfile::tempdir_in("/tmp").unwrap();
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(tempdir.path(), std::fs::Permissions::from_mode(0o700))
                    .unwrap();
            }
            let socket_path = protocol::socket_path_from(Some(
                tempdir.path().join("daemon.sock").into_os_string(),
            ))
            .unwrap();
            let listener = UnixListener::bind(&socket_path).unwrap();
            let state = SupervisorState::shared();
            let (shutdown, shutdown_rx) = watch::channel(false);
            let task = tokio::spawn(run_supervisor_loop(
                listener,
                socket_path.clone(),
                state,
                shutdown,
                shutdown_rx,
            ));
            Self {
                socket_path,
                task,
                _tempdir: tempdir,
            }
        }

        async fn request(&self, command: ProtocolRequest) -> ResponseEnvelope {
            let mut stream = UnixStream::connect(&self.socket_path).await.unwrap();
            protocol::write_frame(
                &mut stream,
                &RequestEnvelope {
                    version: protocol::PROTOCOL_VERSION,
                    command,
                },
            )
            .await
            .unwrap();
            timeout(
                Duration::from_secs(5),
                protocol::read_frame::<_, ResponseEnvelope>(&mut stream),
            )
            .await
            .unwrap()
            .unwrap()
        }

        async fn shutdown(self) {
            let response = self.request(ProtocolRequest::Shutdown).await;
            assert_eq!(
                response,
                success(ProtocolResponse::Shutdown),
                "shutdown response must be readable before cleanup"
            );
            let Self {
                socket_path,
                task,
                _tempdir,
            } = self;
            task.await.unwrap().unwrap();
            assert!(!socket_path.exists(), "daemon socket should be removed");
        }
    }

    fn instance_response(response: ResponseEnvelope) -> InstanceResult {
        match response {
            ResponseEnvelope::Success {
                result: ProtocolResponse::CreateInstance(instance),
                ..
            } => instance,
            other => panic!("expected create response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unix_control_plane_covers_lifecycle_and_isolated_public_listener() {
        let harness = SupervisorHarness::start().await;

        assert_eq!(
            harness.request(ProtocolRequest::Health).await,
            success(ProtocolResponse::Health(HealthResult {
                protocol_version: protocol::PROTOCOL_VERSION,
            }))
        );

        let first = instance_response(
            harness
                .request(ProtocolRequest::CreateInstance(create_request(
                    "first",
                    "127.0.0.1",
                    0,
                )))
                .await,
        );
        let second = instance_response(
            harness
                .request(ProtocolRequest::CreateInstance(create_request(
                    "second",
                    "127.0.0.1",
                    0,
                )))
                .await,
        );
        assert_ne!(first.port, 0);
        assert_ne!(second.port, 0);
        assert_ne!(first.port, second.port);
        assert_eq!(first.address, format!("http://127.0.0.1:{}", first.port));
        assert_eq!(second.address, format!("http://127.0.0.1:{}", second.port));

        let list = harness.request(ProtocolRequest::List).await;
        match list {
            ResponseEnvelope::Success {
                result: ProtocolResponse::List(result),
                ..
            } => assert_eq!(
                result
                    .instances
                    .iter()
                    .map(|instance| instance.name.as_str())
                    .collect::<Vec<_>>(),
                vec!["first", "second"]
            ),
            other => panic!("expected list response, got {other:?}"),
        }

        let conflict = harness
            .request(ProtocolRequest::CreateInstance(create_request(
                "first", "0.0.0.0", first.port,
            )))
            .await;
        assert!(matches!(
            conflict,
            ResponseEnvelope::Error {
                error: ProtocolError {
                    code: ErrorCode::Conflict,
                    ..
                },
                ..
            }
        ));

        let mut public_stream = TcpStream::connect(("127.0.0.1", first.port)).await.unwrap();
        protocol::write_frame(
            &mut public_stream,
            &RequestEnvelope {
                version: protocol::PROTOCOL_VERSION,
                command: ProtocolRequest::Health,
            },
        )
        .await
        .unwrap();
        let public_response = timeout(
            Duration::from_secs(2),
            protocol::read_frame::<_, ResponseEnvelope>(&mut public_stream),
        )
        .await;
        assert!(
            public_response.is_err() || public_response.unwrap().is_err(),
            "public HTTP listener must not expose typed control responses"
        );

        let stopped = harness
            .request(ProtocolRequest::StopInstance(
                protocol::StopInstanceRequest {
                    name: "first".into(),
                },
            ))
            .await;
        assert_eq!(
            stopped,
            success(ProtocolResponse::StopInstance(StopInstanceResult {
                name: "first".into(),
            }))
        );
        let status = harness
            .request(ProtocolRequest::Status(protocol::StatusRequest {
                name: "first".into(),
            }))
            .await;
        assert!(matches!(
            status,
            ResponseEnvelope::Error {
                error: ProtocolError {
                    code: ErrorCode::NotFound,
                    ..
                },
                ..
            }
        ));
        assert!(TcpStream::connect(("127.0.0.1", first.port)).await.is_err());

        harness.shutdown().await;
        assert!(
            TcpStream::connect(("127.0.0.1", second.port))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn concurrent_equal_creates_are_idempotent_over_unix_socket() {
        let harness = SupervisorHarness::start().await;
        let request = ProtocolRequest::CreateInstance(create_request("shared", "127.0.0.1", 0));
        let (left, right) =
            tokio::join!(harness.request(request.clone()), harness.request(request));
        let left = instance_response(left);
        let right = instance_response(right);
        assert_eq!(left, right);
        assert_ne!(left.port, 0);

        let status = harness
            .request(ProtocolRequest::Status(protocol::StatusRequest {
                name: "shared".into(),
            }))
            .await;
        assert_eq!(status, success(ProtocolResponse::Status(left.clone())));

        harness.shutdown().await;
    }

    #[tokio::test]
    async fn create_binds_before_commit_and_reports_port_zero_address() {
        let state = SupervisorState::shared();
        let result = create_instance(&state, create_request("ephemeral", "127.0.0.1", 0))
            .await
            .unwrap();
        assert_eq!(result.name, "ephemeral");
        assert_eq!(result.host, "127.0.0.1");
        assert_ne!(result.port, 0);
        assert_eq!(result.address, format!("http://127.0.0.1:{}", result.port));
        stop_test_instance(&state, "ephemeral").await;
    }

    #[tokio::test]
    async fn equal_creates_are_idempotent_and_conflicts_do_not_bind() {
        let state = SupervisorState::shared();
        let request = create_request("shared", "127.0.0.1", 0);
        let (first, second) = tokio::join!(
            create_instance(&state, request.clone()),
            create_instance(&state, request),
        );
        assert_eq!(first.unwrap(), second.unwrap());
        let conflict = create_instance(&state, create_request("shared", "127.0.0.1", 1))
            .await
            .unwrap_err();
        assert_eq!(conflict.code, ErrorCode::Conflict);
        stop_test_instance(&state, "shared").await;
    }

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

    #[tokio::test]
    async fn stopping_instance_rejects_public_dispatch() {
        let state = SupervisorState::shared();
        let instance = {
            create_instance(&state, create_request("public", "127.0.0.1", 0))
                .await
                .unwrap();
            instance_for(&state, "public").await.unwrap()
        };
        *instance.state.lock().await = InstanceState::Stopping;
        let response = dispatch_instance(
            State(instance),
            Request::builder()
                .uri("/route")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        *state
            .lock()
            .await
            .instances
            .get("public")
            .unwrap()
            .state
            .lock()
            .await = InstanceState::Running;
        stop_test_instance(&state, "public").await;
    }

    #[test]
    fn longest_match_and_uri_rewrite_preserve_query_and_request_metadata() {
        assert!(route_matches("/agent", "/agent/acp"));
        assert!(!route_matches("/agent", "/agent-two/acp"));
        let mut request = Request::builder()
            .method("POST")
            .uri("/agent/acp?x=1")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(17_u8);
        let request = rewrite_route_prefix(request, "/agent").unwrap();
        assert_eq!(request.uri(), "/acp?x=1");
        assert_eq!(request.method(), "POST");
        assert_eq!(request.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(request.extensions().get::<u8>(), Some(&17));
    }

    #[test]
    fn readiness_projection_reports_disabled_without_detail() {
        let result = readiness_result(
            false,
            crate::serve::ReadinessSnapshot {
                attempts: 3,
                failures: 2,
                last_attempt_failed: true,
                last_failure: Some(crate::serve::ReadinessFailure {
                    at: std::time::SystemTime::now(),
                    detail: "ignored when disabled".into(),
                }),
            },
        );

        assert_eq!(result.status, ReadinessStatus::Disabled);
        assert_eq!(result.attempts, 3);
        assert_eq!(result.failures, 2);
        assert_eq!(result.detail, None);
    }

    #[test]
    fn readiness_projection_reports_initial_ready() {
        let result = readiness_result(
            true,
            crate::serve::ReadinessSnapshot {
                attempts: 0,
                failures: 0,
                last_attempt_failed: false,
                last_failure: None,
            },
        );

        assert_eq!(result.status, ReadinessStatus::Ready);
        assert_eq!(result.attempts, 0);
        assert_eq!(result.failures, 0);
        assert_eq!(result.detail, None);
    }

    #[test]
    fn readiness_projection_reports_failed_detail() {
        let result = readiness_result(
            true,
            crate::serve::ReadinessSnapshot {
                attempts: 2,
                failures: 1,
                last_attempt_failed: true,
                last_failure: Some(crate::serve::ReadinessFailure {
                    at: std::time::SystemTime::now(),
                    detail: "spawn failed".into(),
                }),
            },
        );

        assert_eq!(result.status, ReadinessStatus::NotReady);
        assert_eq!(result.attempts, 2);
        assert_eq!(result.failures, 1);
        assert_eq!(result.detail.as_deref(), Some("spawn failed"));
    }

    #[test]
    fn readiness_projection_omits_stale_detail_after_recovery() {
        let result = readiness_result(
            true,
            crate::serve::ReadinessSnapshot {
                attempts: 3,
                failures: 1,
                last_attempt_failed: false,
                last_failure: Some(crate::serve::ReadinessFailure {
                    at: std::time::SystemTime::now(),
                    detail: "stale failure".into(),
                }),
            },
        );

        assert_eq!(result.status, ReadinessStatus::Ready);
        assert_eq!(result.attempts, 3);
        assert_eq!(result.failures, 1);
        assert_eq!(result.detail, None);
    }

    #[test]
    fn public_address_rewrites_unspecified_bind_hosts() {
        assert_eq!(
            public_address("0.0.0.0:8123".parse().unwrap()),
            "http://127.0.0.1:8123"
        );
        assert_eq!(
            public_address("[::]:8123".parse().unwrap()),
            "http://[::1]:8123"
        );
        assert_eq!(
            public_address("192.0.2.1:8123".parse().unwrap()),
            "http://192.0.2.1:8123"
        );
    }

    #[test]
    fn validates_names_routes_and_agent_ids() {
        assert!(validate_name("team.one").is_ok());
        assert!(validate_name("../bad").is_err());
        assert!(validate_route("/team/codex").is_ok());
        assert!(validate_route("/").is_err());
        assert!(validate_route("/bad/").is_err());
        assert!(validate_agent_id("agent").is_ok());
        assert!(validate_agent_id(" ").is_err());
    }
}
