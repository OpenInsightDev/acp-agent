use super::daemon::{
    DaemonInstance, DaemonOperationError, DaemonPhase, SharedSupervisorState, ensure_running,
    instance_for,
};
use super::protocol::{
    ErrorCode, ReadinessResult, ReadinessStatus, RegisterRequest, RegistrationResult,
    RegistrationsResult, UnregisterResult,
};
use super::{validate_agent_id, validate_name, validate_route};
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use std::{
    hash::{Hash, Hasher},
    net::SocketAddr,
    sync::Arc,
};
use tower::ServiceExt;

/// Route key and the metadata needed by control-plane inspection.
#[derive(Debug, Clone)]
pub(super) struct RouteKey {
    pub(super) id: String,
    pub(super) public_route: String,
    pub(super) config: crate::serve::RouteConfig,
}

impl PartialEq for RouteKey {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.public_route == other.public_route
    }
}
impl Eq for RouteKey {}
impl Hash for RouteKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        self.public_route.hash(state);
    }
}

pub(super) fn readiness_result(
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

pub(super) fn registration_result(
    name: &str,
    address: &SocketAddr,
    route_key: &RouteKey,
    runtime: &crate::serve::RouteRuntime,
) -> RegistrationResult {
    RegistrationResult {
        name: name.to_string(),
        id: route_key.id.clone(),
        route: route_key.public_route.clone(),
        path: route_key.config.path.clone(),
        health_endpoint: route_key.config.health_endpoint,
        readyz_endpoint: route_key.config.readyz_endpoint,
        address: super::daemon::public_address(*address),
        max_processes: route_key.config.max_processes,
        readiness: readiness_result(
            route_key.config.readyz_endpoint,
            runtime.readiness_snapshot(),
        ),
    }
}

pub(super) async fn register(
    state: &SharedSupervisorState,
    request: RegisterRequest,
) -> std::result::Result<RegistrationResult, DaemonOperationError> {
    validate_name(&request.name)
        .map_err(|error| DaemonOperationError::new(ErrorCode::InvalidInput, error.to_string()))?;
    if !validate_agent_id(&request.id) {
        return Err(DaemonOperationError::new(
            ErrorCode::InvalidInput,
            "agent id must not be empty",
        ));
    }
    validate_route(&request.route)
        .map_err(|error| DaemonOperationError::new(ErrorCode::InvalidInput, error.to_string()))?;
    let instance = instance_for(state, &request.name).await?;
    ensure_running(&instance).await?;

    crate::serve::validate_route_config(&request.config)
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
    let runtime =
        crate::serve::RouteRuntime::new(resolved, &request.config, instance.cancel.subscribe())
            .map_err(|error| {
                DaemonOperationError::new(
                    ErrorCode::InvalidInput,
                    format!("failed to construct route runtime: {error:#}"),
                )
            })?;
    let route_key = RouteKey {
        id: request.id,
        public_route: request.route,
        config: request.config,
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
    if routes.keys().any(|existing| existing.id == route_key.id) {
        return Err(DaemonOperationError::new(
            ErrorCode::Conflict,
            format!("agent id {} is already registered", route_key.id),
        ));
    }
    if routes
        .keys()
        .any(|existing| existing.public_route == route_key.public_route)
    {
        return Err(DaemonOperationError::new(
            ErrorCode::Conflict,
            format!("route {} is already registered", route_key.public_route),
        ));
    }
    let result = registration_result(&instance.name, &instance.address, &route_key, &runtime);
    routes.insert(route_key, runtime);
    Ok(result)
}

pub(super) async fn unregister(
    state: &SharedSupervisorState,
    request: super::protocol::UnregisterRequest,
) -> std::result::Result<UnregisterResult, DaemonOperationError> {
    validate_name(&request.name)
        .map_err(|error| DaemonOperationError::new(ErrorCode::InvalidInput, error.to_string()))?;
    if !validate_agent_id(&request.id) {
        return Err(DaemonOperationError::new(
            ErrorCode::InvalidInput,
            "agent id must not be empty",
        ));
    }
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

pub(super) async fn registrations(
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

pub(super) async fn dispatch_instance(
    State(instance): State<Arc<DaemonInstance>>,
    request: Request<Body>,
) -> Response {
    let state = instance.state.lock().await;
    if *state != super::protocol::InstanceState::Running {
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
    *request.uri_mut() = Uri::builder()
        .path_and_query(path_and_query)
        .build()
        .map_err(|error| format!("failed to rewrite request URI: {error}"))?;
    Ok(request)
}

pub(super) fn route_matches(route: &str, path: &str) -> bool {
    path == route
        || path
            .strip_prefix(route)
            .is_some_and(|suffix| suffix.starts_with('/'))
}
