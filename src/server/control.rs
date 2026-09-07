use super::daemon::{
    DaemonPhase, SharedSupervisorState, create_instance, list_instances, status_instance,
    stop_all_instances, stop_instance,
};
use super::protocol::{
    self, ErrorCode, HealthResult, ListResult, ProtocolError, Request as ProtocolRequest,
    RequestEnvelope, Response as ProtocolResponse, ResponseEnvelope, StopInstanceResult,
};
use super::routes;
use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

pub(super) async fn run_supervisor_loop(
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
        ProtocolRequest::Register(request) => match routes::register(&state, request).await {
            Ok(result) => success(ProtocolResponse::Register(result)),
            Err(error) => error_response(error.code, error.message),
        },
        ProtocolRequest::Unregister(request) => match routes::unregister(&state, request).await {
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
            match routes::registrations(&state, &request.name).await {
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

pub(super) fn success(result: ProtocolResponse) -> ResponseEnvelope {
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
