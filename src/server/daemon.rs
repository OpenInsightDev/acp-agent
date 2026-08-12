use super::*;

#[derive(Clone)]
pub(super) struct ServerState {
    pub(super) server_name: String,
    pub(super) agents: Arc<RwLock<HashMap<String, RegisteredAgent>>>,
    pub(super) shutdown: watch::Sender<bool>,
}

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

pub(super) struct ForceCloseListener {
    inner: TcpListener,
    force_close: watch::Receiver<bool>,
}

impl axum::serve::Listener for ForceCloseListener {
    type Io = ForceCloseIo;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.inner.accept().await {
                Ok((stream, address)) => {
                    return (ForceCloseIo::new(stream, self.force_close.clone()), address);
                }
                Err(error) => {
                    eprintln!("failed to accept named server connection: {error}");
                    sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

pub(super) struct ForceCloseIo {
    inner: tokio::net::TcpStream,
    cancelled: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl ForceCloseIo {
    fn new(inner: tokio::net::TcpStream, mut force_close: watch::Receiver<bool>) -> Self {
        Self {
            inner,
            cancelled: Box::pin(async move {
                wait_for_shutdown(&mut force_close).await;
            }),
        }
    }

    fn poll_cancelled(&mut self, context: &mut TaskContext<'_>) -> std::io::Result<()> {
        if self.cancelled.as_mut().poll(context).is_ready() {
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "named server shutdown grace expired",
            ))
        } else {
            Ok(())
        }
    }
}

impl AsyncRead for ForceCloseIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if let Err(error) = self.poll_cancelled(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

impl AsyncWrite for ForceCloseIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if let Err(error) = self.poll_cancelled(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.inner).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        if let Err(error) = self.poll_cancelled(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        if let Err(error) = self.poll_cancelled(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

/// Runs the foreground process behind the hidden `__server-run` command.
pub async fn run(name: String, host: String, port: u16) -> Result<()> {
    validate_name(&name)?;
    run_with(name, host, port, ServerPaths::discover()?, SHUTDOWN_GRACE).await
}

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
    write_private_json_exclusive(&state_path, &file, &name)?;
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

pub(super) async fn serve_with_shutdown(
    listener: TcpListener,
    router: Router,
    shutdown_rx: watch::Receiver<bool>,
    shutdown_grace: Duration,
) -> Result<()> {
    let (force_close, force_close_rx) = watch::channel(false);
    let listener = ForceCloseListener {
        inner: listener,
        force_close: force_close_rx,
    };
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
                Err(_) => {
                    force_close.send_replace(true);
                    timeout(Duration::from_secs(1), &mut server)
                        .await
                        .context("named ACP connections did not close after forced shutdown")??;
                    Ok(())
                },
            }
        }
    }
}

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

pub(super) async fn server_status(State(state): State<ServerState>) -> Json<ServerStatus> {
    Json(ServerStatus {
        name: state.server_name,
        pid: std::process::id(),
        version: SERVER_PROTOCOL_VERSION.to_string(),
    })
}

// Readiness is probed by the CLI, keeping this control endpoint cheap.
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
    let router = match crate::serve::agent_router(config, &options) {
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

pub(super) fn serve_options(
    request: &AgentServeRequest,
) -> Result<crate::serve::AgentRouterOptions> {
    let options = crate::serve::AgentRouterOptions {
        path: request.path.clone(),
        cors: crate::serve::cors_options(request.cors_origins.clone(), request.allow_any_origin)?,
        health_endpoint: request.health_endpoint,
        readyz_endpoint: request.readyz_endpoint,
    };
    crate::serve::validate_router_options(&options)?;
    Ok(options)
}

pub(super) async fn resolved_args(id: &str, request: &AgentServeRequest) -> Result<Vec<String>> {
    crate::yolo::resolve_args(id, request.yolo, request.args.clone()).await
}

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

pub(super) async fn shutdown(State(state): State<ServerState>) -> Response {
    state.shutdown.send_replace(true);
    StatusCode::ACCEPTED.into_response()
}

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
