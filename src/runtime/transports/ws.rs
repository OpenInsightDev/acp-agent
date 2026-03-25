//! Transport that exposes an agent over WebSocket + JSON-RPC (stdin/stdout mapped to RPC).
//! It supports a single connection and multiplexes stdout using jsonrpsee subscriptions.
use std::process::ExitStatus;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use jsonrpsee::RpcModule;
use jsonrpsee::core::SubscriptionError;
use jsonrpsee::server::{
    PendingSubscriptionSink, Server, ServerConfig, serve_with_graceful_shutdown, stop_channel,
};
use jsonrpsee::types::ErrorObjectOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::Mutex;

use crate::runtime::prepare::{CommandSpec, PreparedCommand};
use crate::runtime::process::{spawn_stream_child, terminate_child};

const STREAM_BUFFER_CAPACITY: usize = 16;
const COPY_BUFFER_SIZE: usize = 8192;
const WS_WRITE_STDIN_METHOD: &str = "write_stdin";
const WS_CLOSE_STDIN_METHOD: &str = "close_stdin";
const WS_SUBSCRIBE_STDOUT_METHOD: &str = "subscribe_stdout";
const WS_STDOUT_NOTIFICATION_METHOD: &str = "stdout";
const WS_UNSUBSCRIBE_STDOUT_METHOD: &str = "unsubscribe_stdout";

/// Accepts one WebSocket connection and exposes stdin/stdout through RPC methods.
///
/// `write_stdin`/`close_stdin` map to the agent's stdin, while clients subscribe
/// to `stdout` notifications produced from the running process. Only one
/// connection and a limited number of stdout subscriptions are allowed.
pub async fn serve_ws(
    prepared: PreparedCommand,
    subject: &str,
    host: &str,
    port: u16,
) -> Result<ExitStatus> {
    let bound_listener = bind_listener(host, port).await?;
    eprintln!(
        "Running {} over ws://{} (jsonrpsee WebSocket transport)",
        subject, bound_listener.display_address
    );

    let (socket, peer_address) = bound_listener.listener.accept().await.with_context(|| {
        format!(
            "failed to accept WebSocket connection on {}",
            bound_listener.display_address
        )
    })?;
    eprintln!("Accepted connection from {}", peer_address);

    let _temp_dir = prepared.temp_dir;
    serve_ws_connection(prepared.spec, subject, socket).await
}

async fn serve_ws_connection(
    spec: CommandSpec,
    subject: &str,
    socket: TcpStream,
) -> Result<ExitStatus> {
    let mut child = spawn_stream_child(&spec, subject)?;
    let child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("child process missing piped stdin"))?;
    let child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("child process missing piped stdout"))?;

    let session = Arc::new(WsStreamSession::new(child_stdin, child_stdout));
    let module = build_ws_rpc_module(session)?;
    let config = ServerConfig::builder()
        .ws_only()
        .max_connections(1)
        .max_subscriptions_per_connection(8)
        .set_message_buffer_capacity(STREAM_BUFFER_CAPACITY as u32)
        .build();
    let service_builder = Server::builder().set_config(config).to_service_builder();
    let (stop_handle, server_handle) = stop_channel();
    let mut service = service_builder.build(module, stop_handle.clone());
    let session_closed = service.on_session_closed();
    let serve = tokio::spawn(async move {
        serve_with_graceful_shutdown(socket, service, stop_handle.shutdown()).await
    });
    tokio::pin!(session_closed);
    tokio::pin!(serve);

    tokio::select! {
        status = child.wait() => {
            let status = status.context("failed while waiting on child process")?;
            let _ = server_handle.stop();
            let serve_result = serve.as_mut().await.context("failed to join WebSocket transport task")?;
            if let Err(source) = serve_result {
                return Err(anyhow!("WebSocket transport failed: {source}"));
            }
            Ok(status)
        }
        _ = &mut session_closed => {
            let _ = server_handle.stop();
            let status = terminate_child(&mut child).await?;
            let serve_result = serve.as_mut().await.context("failed to join WebSocket transport task")?;
            if let Err(source) = serve_result {
                return Err(anyhow!("WebSocket transport failed: {source}"));
            }
            Ok(status)
        }
    }
}

fn build_ws_rpc_module(session: Arc<WsStreamSession>) -> Result<RpcModule<WsStreamSession>> {
    let mut module = RpcModule::from_arc(session);
    module.register_async_method(WS_WRITE_STDIN_METHOD, |params, session, _| async move {
        let chunk: StreamChunk = params.one().map_err(ErrorObjectOwned::from)?;
        session.write_stdin(&chunk.data).await
    })?;
    module.register_async_method(WS_CLOSE_STDIN_METHOD, |_, session, _| async move {
        session.close_stdin().await
    })?;
    module.register_subscription::<std::result::Result<(), SubscriptionError>, _, _>(
        WS_SUBSCRIBE_STDOUT_METHOD,
        WS_STDOUT_NOTIFICATION_METHOD,
        WS_UNSUBSCRIBE_STDOUT_METHOD,
        |_, pending, session, _| async move { stream_stdout_subscription(pending, session).await },
    )?;
    Ok(module)
}

async fn stream_stdout_subscription(
    pending: PendingSubscriptionSink,
    session: Arc<WsStreamSession>,
) -> std::result::Result<(), SubscriptionError> {
    let Some(mut child_stdout) = session.take_stdout().await else {
        pending
            .reject(ws_internal_error("stdout is already subscribed"))
            .await;
        return Ok(());
    };

    let sink = pending.accept().await?;
    let mut buffer = [0_u8; COPY_BUFFER_SIZE];

    loop {
        tokio::select! {
            _ = sink.closed() => break Ok(()),
            read = child_stdout.read(&mut buffer) => match read {
                Ok(0) => break Ok(()),
                Ok(read) => {
                    let message = serde_json::value::to_raw_value(&StreamChunk {
                        data: buffer[..read].to_vec(),
                    }).map_err(|error| SubscriptionError::from(error.to_string()))?;

                    if sink.send(message).await.is_err() {
                        break Ok(());
                    }
                }
                Err(_) => break Ok(()),
            }
        }
    }
}

fn ws_internal_error(message: impl Into<String>) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(-32000, message.into(), None::<()>)
}

struct WsStreamSession {
    child_stdin: Mutex<Option<ChildStdin>>,
    child_stdout: Mutex<Option<ChildStdout>>,
}

impl WsStreamSession {
    fn new(child_stdin: ChildStdin, child_stdout: ChildStdout) -> Self {
        Self {
            child_stdin: Mutex::new(Some(child_stdin)),
            child_stdout: Mutex::new(Some(child_stdout)),
        }
    }

    async fn write_stdin(&self, data: &[u8]) -> std::result::Result<usize, ErrorObjectOwned> {
        let mut child_stdin = self.child_stdin.lock().await;
        let child_stdin = child_stdin
            .as_mut()
            .ok_or_else(|| ws_internal_error("stdin is closed"))?;

        child_stdin
            .write_all(data)
            .await
            .map_err(|error| ws_internal_error(format!("failed to write stdin: {error}")))?;
        Ok(data.len())
    }

    async fn close_stdin(&self) -> std::result::Result<bool, ErrorObjectOwned> {
        let child_stdin = self.child_stdin.lock().await.take();
        let Some(mut child_stdin) = child_stdin else {
            return Ok(false);
        };

        child_stdin
            .shutdown()
            .await
            .map_err(|error| ws_internal_error(format!("failed to close stdin: {error}")))?;
        Ok(true)
    }

    async fn take_stdout(&self) -> Option<ChildStdout> {
        self.child_stdout.lock().await.take()
    }
}

struct BoundListener {
    listener: TcpListener,
    display_address: String,
}

async fn bind_listener(host: &str, port: u16) -> Result<BoundListener> {
    let listener = TcpListener::bind((host, port)).await.with_context(|| {
        format!(
            "failed to bind WebSocket listener on {}",
            bind_target_display(host, port)
        )
    })?;
    let display_address = listener
        .local_addr()
        .map(|address| address.to_string())
        .unwrap_or_else(|_| bind_target_display(host, port));

    Ok(BoundListener {
        listener,
        display_address,
    })
}

fn bind_target_display(host: &str, port: u16) -> String {
    format!("host={host}, port={port}")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StreamChunk {
    data: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::time::Duration;

    use super::*;
    use jsonrpsee::core::client::{ClientT, SubscriptionClientT};
    use jsonrpsee::rpc_params;
    use jsonrpsee::ws_client::WsClientBuilder;

    #[cfg(unix)]
    async fn run_command_ws_with_listener(
        prepared: PreparedCommand,
        subject: &str,
        listener: BoundListener,
    ) -> Result<ExitStatus> {
        eprintln!(
            "Running {} over ws://{} (jsonrpsee WebSocket transport)",
            subject, listener.display_address
        );

        let (socket, _) = listener.listener.accept().await.with_context(|| {
            format!(
                "failed to accept WebSocket connection on {}",
                listener.display_address
            )
        })?;

        serve_ws_connection(prepared.spec, subject, socket).await
    }

    #[cfg(unix)]
    fn prepared_command_with_program(program: OsString, args: Vec<OsString>) -> PreparedCommand {
        PreparedCommand {
            spec: CommandSpec {
                program,
                args,
                env: Vec::new(),
                current_dir: None,
            },
            temp_dir: None,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ws_transport_streams_full_duplex_over_jsonrpc() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            run_command_ws_with_listener(
                prepared_command_with_program(
                    OsString::from("sh"),
                    vec![
                        OsString::from("-c"),
                        OsString::from("printf 'boot\\n'; cat"),
                    ],
                ),
                "demo-agent",
                BoundListener {
                    listener,
                    display_address: address.to_string(),
                },
            )
            .await
        });

        let client = WsClientBuilder::default()
            .build(format!("ws://{address}"))
            .await
            .unwrap();
        let mut stdout = client
            .subscribe::<StreamChunk, _>(
                WS_SUBSCRIBE_STDOUT_METHOD,
                rpc_params![],
                WS_UNSUBSCRIBE_STDOUT_METHOD,
            )
            .await
            .unwrap();

        let first_chunk = tokio::time::timeout(Duration::from_secs(2), stdout.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(first_chunk.data, b"boot\n");

        let written: usize = client
            .request(
                WS_WRITE_STDIN_METHOD,
                rpc_params![StreamChunk {
                    data: b"ping\n".to_vec(),
                }],
            )
            .await
            .unwrap();
        assert_eq!(written, 5);

        let closed: bool = client
            .request(WS_CLOSE_STDIN_METHOD, rpc_params![])
            .await
            .unwrap();
        assert!(closed);

        let echoed = tokio::time::timeout(Duration::from_secs(2), stdout.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(echoed.data, b"ping\n");

        let status = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ws_transport_rejects_second_stdout_subscription() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            run_command_ws_with_listener(
                prepared_command_with_program(
                    OsString::from("sh"),
                    vec![OsString::from("-c"), OsString::from("cat")],
                ),
                "demo-agent",
                BoundListener {
                    listener,
                    display_address: address.to_string(),
                },
            )
            .await
        });

        let client = WsClientBuilder::default()
            .build(format!("ws://{address}"))
            .await
            .unwrap();
        let first_subscription = client
            .subscribe::<StreamChunk, _>(
                WS_SUBSCRIBE_STDOUT_METHOD,
                rpc_params![],
                WS_UNSUBSCRIBE_STDOUT_METHOD,
            )
            .await;
        assert!(first_subscription.is_ok());

        let second_subscription = client
            .subscribe::<StreamChunk, _>(
                WS_SUBSCRIBE_STDOUT_METHOD,
                rpc_params![],
                WS_UNSUBSCRIBE_STDOUT_METHOD,
            )
            .await;
        assert!(second_subscription.is_err());

        let closed: bool = client
            .request(WS_CLOSE_STDIN_METHOD, rpc_params![])
            .await
            .unwrap();
        assert!(closed);
        drop(first_subscription);

        let status = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(status.success());
    }
}
