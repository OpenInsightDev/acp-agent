use super::control;

use super::protocol::{self, CreateInstanceRequest, ErrorCode, InstanceResult, InstanceState};
use super::{routes, validate_name};
use anyhow::Result;
use axum::Router;

use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use tokio::net::TcpListener;
use tokio::sync::{Mutex as AsyncMutex, RwLock, watch};

const INSTANCE_STOP_GRACE: Duration = super::SHUTDOWN_GRACE;

pub(super) fn public_address(address: SocketAddr) -> String {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DaemonPhase {
    Running,
    ShuttingDown,
}

pub(super) struct SupervisorState {
    pub(super) instances: HashMap<String, Arc<DaemonInstance>>,
    pub(super) phase: DaemonPhase,
}

pub(super) type SharedSupervisorState = Arc<AsyncMutex<SupervisorState>>;

impl SupervisorState {
    fn shared() -> SharedSupervisorState {
        Arc::new(AsyncMutex::new(Self {
            instances: HashMap::new(),
            phase: DaemonPhase::Running,
        }))
    }
}

pub(super) struct DaemonInstance {
    pub(super) name: String,
    pub(super) requested_host: String,
    pub(super) requested_port: u16,
    pub(super) address: SocketAddr,
    pub(super) state: AsyncMutex<InstanceState>,
    shutdown: watch::Sender<bool>,
    pub(super) cancel: watch::Sender<bool>,
    listener_task: AsyncMutex<Option<tokio::task::JoinHandle<Result<()>>>>,
    pub(super) stop_lock: AsyncMutex<()>,
    pub(super) routes: RwLock<HashMap<routes::RouteKey, Arc<crate::serve::RouteRuntime>>>,
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
    control::run_supervisor_loop(listener, socket_path, state, shutdown, shutdown_rx).await
}

#[derive(Debug)]
pub(super) struct DaemonOperationError {
    pub(super) code: ErrorCode,
    pub(super) message: String,
}

impl DaemonOperationError {
    pub(super) fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

pub(super) async fn create_instance(
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
        .fallback(routes::dispatch_instance)
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

pub(super) async fn list_instances(state: &SharedSupervisorState) -> Vec<InstanceResult> {
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

pub(super) async fn status_instance(
    state: &SharedSupervisorState,
    name: &str,
) -> std::result::Result<InstanceResult, DaemonOperationError> {
    let instance = instance_for(state, name).await?;
    let current = *instance.state.lock().await;
    Ok(instance.result(current))
}

pub(super) async fn stop_instance(
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

pub(super) async fn stop_all_instances(state: &SharedSupervisorState) {
    let names = {
        let daemon = state.lock().await;
        daemon.instances.keys().cloned().collect::<Vec<_>>()
    };
    for name in names {
        let _ = stop_instance(state, &name).await;
    }
}

pub(super) async fn instance_for(
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

pub(super) async fn ensure_running(
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

#[cfg(test)]
mod tests {
    use super::super::control::{run_supervisor_loop, success};
    use super::super::routes::{
        dispatch_instance, readiness_result, rewrite_route_prefix, route_matches,
    };
    use super::super::{validate_agent_id, validate_name, validate_route};
    use super::protocol::{
        self, CreateInstanceRequest, ErrorCode, HealthResult, InstanceResult, InstanceState,
        ProtocolError, ReadinessStatus, Request as ProtocolRequest, RequestEnvelope,
        Response as ProtocolResponse, ResponseEnvelope, StopInstanceResult,
    };
    use super::public_address;
    use super::{
        SharedSupervisorState, SupervisorState, create_instance, instance_for, stop_instance,
    };
    use anyhow::Result;
    use axum::http::header;
    use axum::{
        body::Body,
        extract::State,
        http::{Request, StatusCode},
    };
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::net::UnixStream;
    use tokio::net::{TcpListener, TcpStream, UnixListener};
    use tokio::sync::watch;
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
        assert!(validate_agent_id("agent"));
        assert!(!validate_agent_id(" "));
    }
}
