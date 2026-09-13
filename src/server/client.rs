use super::protocol::{
    self, CreateInstanceRequest, ErrorCode, InstanceResult, InstanceState, ProtocolError,
    ReadinessStatus, RegisterRequest, RegistrationResult, RegistrationsRequest,
    Request as ProtocolRequest, RequestEnvelope, Response as ProtocolResponse, ResponseEnvelope,
    StatusRequest, StopInstanceRequest, UnregisterRequest,
};
use super::{
    RegisterOptions, RegisterResult, RegistrationRecord, ServerRecord, StartOptions, StartResult,
    StopResult, UnregisterResult, validate_name, validate_route,
};

use anyhow::{Context, Result, anyhow, bail};
use std::{
    fmt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};
use tokio::time::{sleep, timeout};

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
struct MissingDaemonEndpoint {
    path: PathBuf,
    source: std::io::Error,
}

impl fmt::Display for MissingDaemonEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "daemon endpoint {} is missing: {}",
            self.path.display(),
            self.source
        )
    }
}

impl std::error::Error for MissingDaemonEndpoint {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Debug)]
struct DaemonEndpointError {
    path: PathBuf,
    source: std::io::Error,
}

impl fmt::Display for DaemonEndpointError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "daemon endpoint {} is unusable: {}",
            self.path.display(),
            self.source
        )
    }
}

impl std::error::Error for DaemonEndpointError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Debug)]
struct DaemonClientError {
    code: ErrorCode,
    message: String,
}

impl fmt::Display for DaemonClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "daemon {} error: {}",
            error_code_name(self.code),
            self.message
        )
    }
}

impl std::error::Error for DaemonClientError {}

/// Keeps a daemon child owned by this client until the daemon answers health.
struct StartupChildGuard {
    child: Option<Child>,
}

impl StartupChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child
            .as_mut()
            .expect("startup child guard was already disarmed")
    }

    fn disarm(mut self) -> Child {
        self.child
            .take()
            .expect("startup child guard was already disarmed")
    }

    fn cleanup(&mut self) -> Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let _ = child.kill();
        child
            .wait()
            .context("failed to wait for daemon child during startup cleanup")?;
        Ok(())
    }
}

impl Drop for StartupChildGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        // A cancelled startup future cannot run its async cleanup branch. Kill
        // and reap synchronously so cancellation cannot orphan this child.
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Starts a named ACP server through the daemon control protocol.
pub async fn start(options: StartOptions) -> Result<StartResult> {
    validate_name(&options.name)?;
    let response = request_or_start(ProtocolRequest::CreateInstance(CreateInstanceRequest {
        name: options.name,
        host: options.host,
        port: options.port,
    }))
    .await?;
    let instance = expect_create_instance(response)?;
    Ok(StartResult {
        name: instance.name,
        address: instance.address,
    })
}

/// Stops a named ACP server through the daemon control protocol.
pub async fn stop(name: &str) -> Result<StopResult> {
    validate_name(name)?;
    let response = request_existing(ProtocolRequest::StopInstance(StopInstanceRequest {
        name: name.to_string(),
    }))
    .await?;
    let result = match response {
        ProtocolResponse::StopInstance(result) => result,
        other => bail!("daemon returned {other:?} for StopInstance"),
    };
    Ok(StopResult { name: result.name })
}

/// Registers an agent route through the daemon control protocol.
pub async fn register(agent_id: &str, options: RegisterOptions) -> Result<RegisterResult> {
    validate_name(&options.name)?;
    let route = options.route.unwrap_or_else(|| format!("/{agent_id}"));
    validate_route(&route)?;
    let response = request_existing(ProtocolRequest::Register(RegisterRequest {
        name: options.name,
        id: agent_id.to_string(),
        route: route.clone(),
        config: options.config,
        yolo: options.yolo,
        args: options.args,
    }))
    .await?;
    let registration = match response {
        ProtocolResponse::Register(result) => result,
        other => bail!("daemon returned {other:?} for Register"),
    };
    Ok(RegisterResult {
        agent_id: registration.id,
        route: registration.route,
        address: registration.address,
    })
}

/// Removes an agent route through the daemon control protocol.
pub async fn unregister(agent_id: &str, name: &str) -> Result<UnregisterResult> {
    validate_name(name)?;
    let response = request_existing(ProtocolRequest::Unregister(UnregisterRequest {
        name: name.to_string(),
        id: agent_id.to_string(),
    }))
    .await?;
    let result = match response {
        ProtocolResponse::Unregister(result) => result,
        other => bail!("daemon returned {other:?} for Unregister"),
    };
    Ok(UnregisterResult {
        agent_id: result.id,
        server_name: result.name,
    })
}

/// Lists daemon-owned named server instances.
pub async fn list() -> Result<Vec<ServerRecord>> {
    let response = request_existing(ProtocolRequest::List).await?;
    let result = match response {
        ProtocolResponse::List(result) => result,
        other => bail!("daemon returned {other:?} for List"),
    };
    Ok(result.instances.iter().map(server_record).collect())
}

/// Reports one daemon-owned named server instance.
pub async fn status(name: &str) -> Result<ServerRecord> {
    validate_name(name)?;
    let response = request_existing(ProtocolRequest::Status(StatusRequest {
        name: name.to_string(),
    }))
    .await?;
    let instance = expect_status_instance(response)?;
    Ok(server_record(&instance))
}

/// Lists registrations through the daemon control protocol.
pub async fn registrations(name: &str) -> Result<Vec<RegistrationRecord>> {
    validate_name(name)?;
    let response = request_existing(ProtocolRequest::Registrations(RegistrationsRequest {
        name: name.to_string(),
    }))
    .await?;
    let result = match response {
        ProtocolResponse::Registrations(result) => result,
        other => bail!("daemon returned {other:?} for Registrations"),
    };
    Ok(result
        .registrations
        .into_iter()
        .map(registration_record)
        .collect())
}

fn server_record(instance: &InstanceResult) -> ServerRecord {
    ServerRecord {
        name: instance.name.clone(),
        state: instance_state_name(instance.state).to_string(),
        host: instance.host.clone(),
        port: instance.port,
        address: instance.address.clone(),
    }
}

fn instance_state_name(state: InstanceState) -> &'static str {
    match state {
        InstanceState::Running => "running",
        InstanceState::Stopping => "stopping",
    }
}

fn registration_record(registration: RegistrationResult) -> RegistrationRecord {
    RegistrationRecord {
        id: registration.id,
        route: registration.route,
        readiness: readiness_status_name(registration.readiness.status).to_string(),
        detail: registration.readiness.detail,
    }
}

fn readiness_status_name(status: ReadinessStatus) -> &'static str {
    match status {
        ReadinessStatus::Disabled => "disabled",
        ReadinessStatus::Ready => "ready",
        ReadinessStatus::NotReady => "not_ready",
    }
}

fn expect_create_instance(response: ProtocolResponse) -> Result<InstanceResult> {
    match response {
        ProtocolResponse::CreateInstance(instance) => Ok(instance),
        other => bail!("daemon returned {other:?} for CreateInstance"),
    }
}

fn expect_status_instance(response: ProtocolResponse) -> Result<InstanceResult> {
    match response {
        ProtocolResponse::Status(instance) => Ok(instance),
        other => bail!("daemon returned {other:?} for Status"),
    }
}

fn error_code_name(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::Unavailable => "unavailable",
        ErrorCode::InvalidInput => "invalid_input",
        ErrorCode::NotFound => "not_found",
        ErrorCode::Conflict => "conflict",
        ErrorCode::Operation => "operation",
    }
}

async fn request_or_start(command: ProtocolRequest) -> Result<ProtocolResponse> {
    match request_existing(command.clone()).await {
        Ok(response) => return Ok(response),
        Err(error) if error.downcast_ref::<MissingDaemonEndpoint>().is_some() => {}
        Err(error) => return Err(error),
    }
    start_daemon().await?;
    request_existing(command).await
}

async fn request_existing(command: ProtocolRequest) -> Result<ProtocolResponse> {
    let path = protocol::daemon_socket_path().context("failed to resolve daemon endpoint")?;
    let mut stream = tokio::net::UnixStream::connect(&path)
        .await
        .map_err(|source| {
            if matches!(
                source.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) {
                anyhow::Error::new(MissingDaemonEndpoint { path, source })
            } else {
                anyhow::Error::new(DaemonEndpointError { path, source })
            }
        })?;
    protocol::write_frame(
        &mut stream,
        &RequestEnvelope {
            version: protocol::PROTOCOL_VERSION,
            command,
        },
    )
    .await?;
    let response: ResponseEnvelope = protocol::read_frame(&mut stream).await?;
    response_result(response)
}

async fn start_daemon() -> Result<()> {
    let executable = std::env::current_exe().context("failed to locate acp-agent executable")?;
    let child = Command::new(executable)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to start acp-agent daemon")?;
    let mut guard = StartupChildGuard::new(child);
    let health = match timeout(DAEMON_START_TIMEOUT, wait_for_health(&mut guard)).await {
        Ok(result) => result,
        Err(_) => Err(anyhow!(
            "timed out waiting for the acp-agent daemon to become healthy"
        )),
    };
    if let Err(error) = health {
        let cleanup = guard.cleanup();
        return match cleanup {
            Ok(()) => Err(error),
            Err(cleanup_error) => Err(error.context(format!(
                "failed to clean up daemon after startup failure: {cleanup_error:#}"
            ))),
        };
    }
    drop(guard.disarm());
    Ok(())
}

async fn wait_for_health(guard: &mut StartupChildGuard) -> Result<()> {
    let mut child_exited = false;
    let mut waiting_for_winner = false;
    loop {
        match request_existing(ProtocolRequest::Health).await {
            Ok(ProtocolResponse::Health(result)) => {
                if result.protocol_version != protocol::PROTOCOL_VERSION {
                    bail!(
                        "daemon reported protocol version {}, expected {}",
                        result.protocol_version,
                        protocol::PROTOCOL_VERSION
                    );
                }
                return Ok(());
            }
            Ok(other) => bail!("daemon returned {other:?} for Health"),
            Err(error) if error.downcast_ref::<MissingDaemonEndpoint>().is_some() => {}
            Err(error) if error.downcast_ref::<DaemonEndpointError>().is_some() => {}
            Err(error) => return Err(error),
        }

        if !child_exited {
            if let Some(status) = guard
                .child_mut()
                .try_wait()
                .context("failed to inspect daemon startup process")?
            {
                child_exited = true;
                if !status.success() {
                    // A concurrent starter may have won the endpoint race. Keep
                    // checking until the startup timeout for its health response.
                    waiting_for_winner = true;
                } else {
                    bail!("acp-agent daemon exited before health succeeded");
                }
            }
        } else if !waiting_for_winner {
            bail!("acp-agent daemon exited before health succeeded");
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn response_result(response: ResponseEnvelope) -> Result<ProtocolResponse> {
    match response {
        ResponseEnvelope::Success { version, result } => {
            if version != protocol::PROTOCOL_VERSION {
                bail!(
                    "daemon response used protocol version {version}, expected {}",
                    protocol::PROTOCOL_VERSION
                );
            }
            Ok(result)
        }
        ResponseEnvelope::Error { version, error } => {
            if version != protocol::PROTOCOL_VERSION {
                bail!(
                    "daemon error response used protocol version {version}, expected {}",
                    protocol::PROTOCOL_VERSION
                );
            }
            Err(protocol_error(error))
        }
    }
}

fn protocol_error(error: ProtocolError) -> anyhow::Error {
    anyhow::Error::new(DaemonClientError {
        code: error.code,
        message: error.message,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        DaemonClientError, ErrorCode, InstanceResult, InstanceState, ReadinessStatus,
        RegistrationResult, protocol_error, registration_record, server_record,
    };
    use crate::server::protocol::{ProtocolError, ReadinessResult};

    #[test]
    fn maps_protocol_errors_to_stable_typed_messages() {
        let error = protocol_error(ProtocolError {
            code: ErrorCode::NotFound,
            message: "missing".to_string(),
        });
        assert_eq!(error.to_string(), "daemon not_found error: missing");
        assert_eq!(
            error
                .downcast_ref::<DaemonClientError>()
                .map(|error| error.code),
            Some(ErrorCode::NotFound)
        );
    }

    #[test]
    fn maps_instance_states_without_legacy_process_fields() {
        let record = server_record(&InstanceResult {
            name: "default".to_string(),
            host: "127.0.0.1".to_string(),
            port: 8010,
            address: "http://127.0.0.1:8010".to_string(),
            state: InstanceState::Running,
        });
        assert_eq!(record.state, "running");
        assert_eq!(record.host, "127.0.0.1");
        assert_eq!(record.port, 8010);
        assert_eq!(record.address, "http://127.0.0.1:8010");
    }

    #[test]
    fn maps_readiness_status_and_detail() {
        let record = registration_record(RegistrationResult {
            name: "work".into(),
            id: "demo".into(),
            route: "/demo".into(),
            address: "http://127.0.0.1:8010".into(),
            path: "/acp".into(),
            health_endpoint: true,
            readyz_endpoint: true,
            max_processes: 1,
            readiness: ReadinessResult {
                status: ReadinessStatus::NotReady,
                attempts: 2,
                failures: 1,
                detail: Some("spawn failed".into()),
            },
        });

        assert_eq!(record.id, "demo");
        assert_eq!(record.route, "/demo");
        assert_eq!(record.readiness, "not_ready");
        assert_eq!(record.detail.as_deref(), Some("spawn failed"));
    }
}
