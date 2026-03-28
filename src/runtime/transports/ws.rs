//! Transport that exposes an agent over WebSocket + JSON-RPC (stdin/stdout mapped to RPC).
//! It supports a single connection and multiplexes stdout using jsonrpsee subscriptions.
use std::pin::Pin;
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
use tokio::sync::Mutex;

use crate::runtime::prepare::{CommandSpec, PreparedCommand};
use crate::runtime::process::{spawn_stream_child, terminate_child};

const STREAM_BUFFER_CAPACITY: usize = 16;
const COPY_BUFFER_SIZE: usize = 8192;
#[doc(hidden)]
pub const WS_WRITE_STDIN_METHOD: &str = "write_stdin";
#[doc(hidden)]
pub const WS_CLOSE_STDIN_METHOD: &str = "close_stdin";
#[doc(hidden)]
pub const WS_SUBSCRIBE_STDOUT_METHOD: &str = "subscribe_stdout";
#[doc(hidden)]
pub const WS_STDOUT_NOTIFICATION_METHOD: &str = "stdout";
#[doc(hidden)]
pub const WS_UNSUBSCRIBE_STDOUT_METHOD: &str = "unsubscribe_stdout";

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

#[doc(hidden)]
pub async fn serve_ws_connection(
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
    child_stdin: Mutex<Option<Pin<Box<dyn tokio::io::AsyncWrite + Send>>>>,
    child_stdout: Mutex<Option<Pin<Box<dyn tokio::io::AsyncRead + Send>>>>,
}

impl WsStreamSession {
    fn new<Stdin, Stdout>(child_stdin: Stdin, child_stdout: Stdout) -> Self
    where
        Stdin: tokio::io::AsyncWrite + Send + 'static,
        Stdout: tokio::io::AsyncRead + Send + 'static,
    {
        Self {
            child_stdin: Mutex::new(Some(Box::pin(child_stdin))),
            child_stdout: Mutex::new(Some(Box::pin(child_stdout))),
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

    async fn take_stdout(&self) -> Option<Pin<Box<dyn tokio::io::AsyncRead + Send>>> {
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

/// Binary chunk payload used by the WebSocket JSON-RPC stream methods.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamChunk {
    /// Bytes carried over the WebSocket JSON-RPC stream payload.
    pub data: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::io;
    use std::rc::Rc;
    use std::task::{Context as TaskContext, Poll};
    use std::time::Duration;

    use super::*;
    use agent_client_protocol::{self as acp, Agent as _, Client as _};
    use anyhow::Context as _;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use tokio::sync::{Mutex as TokioMutex, mpsc, oneshot};
    use tokio::task::LocalSet;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_util::compat::{FuturesAsyncReadCompatExt, FuturesAsyncWriteCompatExt};

    async fn run_stream_ws_with_listener<Stdin, Stdout>(
        stdin: Stdin,
        stdout: Stdout,
        subject: &str,
        listener: BoundListener,
    ) -> Result<()>
    where
        Stdin: tokio::io::AsyncWrite + Send + 'static,
        Stdout: tokio::io::AsyncRead + Send + 'static,
    {
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

        let session = Arc::new(WsStreamSession::new(stdin, stdout));
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

        let _ = (&mut session_closed).await;
        let _ = server_handle.stop();
        let serve_result = serve
            .as_mut()
            .await
            .context("failed to join WebSocket transport task")?;
        if let Err(source) = serve_result {
            return Err(anyhow!("WebSocket transport failed: {source}"));
        }

        Ok(())
    }

    #[derive(Clone, Debug, Default)]
    struct RecordingClient {
        notifications: Arc<TokioMutex<Vec<acp::SessionNotification>>>,
    }

    impl RecordingClient {
        async fn first_notification(&self) -> Option<acp::SessionNotification> {
            self.notifications.lock().await.first().cloned()
        }
    }

    #[async_trait::async_trait(?Send)]
    impl acp::Client for RecordingClient {
        async fn request_permission(
            &self,
            _args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn write_text_file(
            &self,
            _args: acp::WriteTextFileRequest,
        ) -> acp::Result<acp::WriteTextFileResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn read_text_file(
            &self,
            _args: acp::ReadTextFileRequest,
        ) -> acp::Result<acp::ReadTextFileResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn create_terminal(
            &self,
            _args: acp::CreateTerminalRequest,
        ) -> acp::Result<acp::CreateTerminalResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn terminal_output(
            &self,
            _args: acp::TerminalOutputRequest,
        ) -> acp::Result<acp::TerminalOutputResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn release_terminal(
            &self,
            _args: acp::ReleaseTerminalRequest,
        ) -> acp::Result<acp::ReleaseTerminalResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn wait_for_terminal_exit(
            &self,
            _args: acp::WaitForTerminalExitRequest,
        ) -> acp::Result<acp::WaitForTerminalExitResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn kill_terminal(
            &self,
            _args: acp::KillTerminalRequest,
        ) -> acp::Result<acp::KillTerminalResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
            self.notifications.lock().await.push(args);
            Ok(())
        }

        async fn ext_method(&self, _args: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn ext_notification(&self, _args: acp::ExtNotification) -> acp::Result<()> {
            Err(acp::Error::method_not_found())
        }
    }

    struct TestAcpAgent {
        notifications: mpsc::UnboundedSender<(acp::SessionNotification, oneshot::Sender<()>)>,
        next_session_id: Cell<u64>,
    }

    impl TestAcpAgent {
        fn new(
            notifications: mpsc::UnboundedSender<(acp::SessionNotification, oneshot::Sender<()>)>,
        ) -> Self {
            Self {
                notifications,
                next_session_id: Cell::new(0),
            }
        }
    }

    #[async_trait::async_trait(?Send)]
    impl acp::Agent for TestAcpAgent {
        async fn initialize(
            &self,
            arguments: acp::InitializeRequest,
        ) -> acp::Result<acp::InitializeResponse> {
            Ok(
                acp::InitializeResponse::new(arguments.protocol_version).agent_info(
                    acp::Implementation::new("ws-test-agent", "0.1.0").title("WS Test Agent"),
                ),
            )
        }

        async fn authenticate(
            &self,
            _arguments: acp::AuthenticateRequest,
        ) -> acp::Result<acp::AuthenticateResponse> {
            Ok(acp::AuthenticateResponse::default())
        }

        async fn new_session(
            &self,
            _arguments: acp::NewSessionRequest,
        ) -> acp::Result<acp::NewSessionResponse> {
            let session_id = self.next_session_id.get();
            self.next_session_id.set(session_id + 1);
            Ok(acp::NewSessionResponse::new(session_id.to_string()))
        }

        async fn load_session(
            &self,
            _arguments: acp::LoadSessionRequest,
        ) -> acp::Result<acp::LoadSessionResponse> {
            Ok(acp::LoadSessionResponse::new())
        }

        async fn set_session_mode(
            &self,
            _arguments: acp::SetSessionModeRequest,
        ) -> acp::Result<acp::SetSessionModeResponse> {
            Ok(acp::SetSessionModeResponse::new())
        }

        async fn prompt(&self, arguments: acp::PromptRequest) -> acp::Result<acp::PromptResponse> {
            let (ack_tx, ack_rx) = oneshot::channel();
            self.notifications
                .send((
                    acp::SessionNotification::new(
                        arguments.session_id,
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                            "hello over ws bridge".into(),
                        )),
                    ),
                    ack_tx,
                ))
                .map_err(|_| acp::Error::internal_error())?;
            let _ = ack_rx.await;
            Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))
        }

        async fn cancel(&self, _args: acp::CancelNotification) -> acp::Result<()> {
            Ok(())
        }

        async fn set_session_config_option(
            &self,
            _args: acp::SetSessionConfigOptionRequest,
        ) -> acp::Result<acp::SetSessionConfigOptionResponse> {
            Ok(acp::SetSessionConfigOptionResponse::new(Vec::new()))
        }

        async fn list_sessions(
            &self,
            _args: acp::ListSessionsRequest,
        ) -> acp::Result<acp::ListSessionsResponse> {
            Ok(acp::ListSessionsResponse::new(Vec::new()))
        }

        async fn ext_method(&self, _args: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn ext_notification(&self, _args: acp::ExtNotification) -> acp::Result<()> {
            Err(acp::Error::method_not_found())
        }
    }

    fn spawn_test_agent_streams() -> (
        impl tokio::io::AsyncWrite + Send,
        impl tokio::io::AsyncRead + Send,
    ) {
        let (client_to_agent_rx, client_to_agent_tx) = piper::pipe(1024);
        let (agent_to_client_rx, agent_to_client_tx) = piper::pipe(1024);
        let (notification_tx, mut notification_rx) = mpsc::unbounded_channel();

        let (conn, io_task) = acp::AgentSideConnection::new(
            TestAcpAgent::new(notification_tx),
            agent_to_client_tx,
            client_to_agent_rx,
            |fut| {
                tokio::task::spawn_local(fut);
            },
        );
        let conn = Rc::new(conn);
        let notification_conn = Rc::clone(&conn);

        tokio::task::spawn_local(io_task);
        tokio::task::spawn_local(async move {
            while let Some((notification, ack)) = notification_rx.recv().await {
                if notification_conn
                    .session_notification(notification)
                    .await
                    .is_err()
                {
                    break;
                }
                let _ = ack.send(());
            }
        });

        (
            client_to_agent_tx.compat_write(),
            agent_to_client_rx.compat(),
        )
    }

    enum BridgeCommand {
        Request {
            method: String,
            params: Value,
            response: Option<oneshot::Sender<Result<Value>>>,
        },
        Shutdown,
    }

    struct WsRpcBridgeHandle {
        command_tx: mpsc::UnboundedSender<BridgeCommand>,
        manager: tokio::task::JoinHandle<Result<()>>,
    }

    impl WsRpcBridgeHandle {
        async fn shutdown(self) -> Result<()> {
            let _ = self.command_tx.send(BridgeCommand::Shutdown);
            self.manager
                .await
                .context("failed to join websocket bridge task")?
        }
    }

    struct WsRpcReader {
        incoming_rx: mpsc::UnboundedReceiver<Vec<u8>>,
        buffer: Vec<u8>,
        offset: usize,
    }

    impl WsRpcReader {
        fn new(incoming_rx: mpsc::UnboundedReceiver<Vec<u8>>) -> Self {
            Self {
                incoming_rx,
                buffer: Vec::new(),
                offset: 0,
            }
        }
    }

    impl futures_util::io::AsyncRead for WsRpcReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
            buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            if self.offset < self.buffer.len() {
                let remaining = &self.buffer[self.offset..];
                let read = remaining.len().min(buf.len());
                buf[..read].copy_from_slice(&remaining[..read]);
                self.offset += read;
                if self.offset == self.buffer.len() {
                    self.buffer.clear();
                    self.offset = 0;
                }
                return Poll::Ready(Ok(read));
            }

            match self.incoming_rx.poll_recv(cx) {
                Poll::Ready(Some(chunk)) => {
                    self.buffer = chunk;
                    self.offset = 0;
                    self.poll_read(cx, buf)
                }
                Poll::Ready(None) => Poll::Ready(Ok(0)),
                Poll::Pending => Poll::Pending,
            }
        }
    }

    struct WsRpcWriter {
        command_tx: mpsc::UnboundedSender<BridgeCommand>,
        buffer: Vec<u8>,
    }

    impl WsRpcWriter {
        fn new(command_tx: mpsc::UnboundedSender<BridgeCommand>) -> Self {
            Self {
                command_tx,
                buffer: Vec::new(),
            }
        }

        fn queue_complete_lines(&mut self) -> io::Result<()> {
            while let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let chunk = self.buffer.drain(..=position).collect::<Vec<_>>();
                self.command_tx
                    .send(BridgeCommand::Request {
                        method: WS_WRITE_STDIN_METHOD.to_string(),
                        params: json!([{ "data": chunk }]),
                        response: None,
                    })
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "websocket bridge closed")
                    })?;
            }

            Ok(())
        }
    }

    impl futures_util::io::AsyncWrite for WsRpcWriter {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.buffer.extend_from_slice(buf);
            if let Err(error) = self.queue_complete_lines() {
                return Poll::Ready(Err(error));
            }
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<io::Result<()>> {
            if !self.buffer.is_empty() {
                let chunk = std::mem::take(&mut self.buffer);
                self.command_tx
                    .send(BridgeCommand::Request {
                        method: WS_WRITE_STDIN_METHOD.to_string(),
                        params: json!([{ "data": chunk }]),
                        response: None,
                    })
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "websocket bridge closed")
                    })?;
            }
            let _ = self.command_tx.send(BridgeCommand::Request {
                method: WS_CLOSE_STDIN_METHOD.to_string(),
                params: json!([]),
                response: None,
            });
            Poll::Ready(Ok(()))
        }
    }

    async fn bridge_request(
        command_tx: &mpsc::UnboundedSender<BridgeCommand>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let (response_tx, response_rx) = oneshot::channel();
        command_tx
            .send(BridgeCommand::Request {
                method: method.to_string(),
                params,
                response: Some(response_tx),
            })
            .map_err(|_| anyhow!("websocket bridge command channel closed"))?;
        response_rx
            .await
            .map_err(|_| anyhow!("websocket bridge response channel closed"))?
    }

    async fn connect_sdk_bridge(
        url: &str,
    ) -> Result<(WsRpcWriter, WsRpcReader, WsRpcBridgeHandle)> {
        let (websocket, _) = connect_async(url)
            .await
            .with_context(|| format!("failed to connect websocket bridge to {url}"))?;
        let (mut sink, mut stream) = websocket.split();
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();

        let manager = tokio::spawn(async move {
            let mut next_id = 0_u64;
            let mut pending: HashMap<u64, Option<oneshot::Sender<Result<Value>>>> = HashMap::new();

            loop {
                tokio::select! {
                    command = command_rx.recv() => match command {
                        Some(BridgeCommand::Request { method, params, response }) => {
                            let id = next_id;
                            next_id += 1;
                            pending.insert(id, response);
                            let request = json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "method": method,
                                "params": params,
                            });
                            sink.send(Message::Text(request.to_string().into()))
                                .await
                                .map_err(|error| anyhow!("failed to send websocket request: {error}"))?;
                        }
                        Some(BridgeCommand::Shutdown) => {
                            let _ = sink.close().await;
                            break;
                        }
                        None => break,
                    },
                    message = stream.next() => match message {
                        Some(Ok(Message::Text(text))) => {
                            let payload: Value = serde_json::from_str(&text)
                                .with_context(|| format!("failed to decode websocket payload: {text}"))?;
                            if let Some(id) = payload.get("id").and_then(Value::as_u64) {
                                if let Some(response) = pending.remove(&id).flatten() {
                                    if let Some(result) = payload.get("result") {
                                        let _ = response.send(Ok(result.clone()));
                                    } else if let Some(error) = payload.get("error") {
                                        let _ = response.send(Err(anyhow!("websocket rpc error: {error}")));
                                    } else {
                                        let _ = response.send(Ok(Value::Null));
                                    }
                                }
                            } else if payload.get("method").and_then(Value::as_str)
                                == Some(WS_STDOUT_NOTIFICATION_METHOD)
                            {
                                let bytes = payload
                                    .get("params")
                                    .and_then(|params| params.get("result"))
                                    .and_then(|result| result.get("data"))
                                    .and_then(Value::as_array)
                                    .map(|items| {
                                        items
                                            .iter()
                                            .filter_map(Value::as_u64)
                                            .map(|value| value as u8)
                                            .collect::<Vec<_>>()
                                    })
                                    .ok_or_else(|| anyhow!("missing stdout payload in websocket notification"))?;
                                let _ = incoming_tx.send(bytes);
                            }
                        }
                        Some(Ok(Message::Binary(_))) => {}
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(Message::Frame(_))) => {}
                        Some(Err(error)) => return Err(anyhow!("websocket bridge read failed: {error}")),
                    }
                }
            }

            Ok(())
        });

        bridge_request(&command_tx, WS_SUBSCRIBE_STDOUT_METHOD, json!([])).await?;

        Ok((
            WsRpcWriter::new(command_tx.clone()),
            WsRpcReader::new(incoming_rx),
            WsRpcBridgeHandle {
                command_tx,
                manager,
            },
        ))
    }

    async fn wait_for_session_notification(
        client: &RecordingClient,
        timeout: Duration,
    ) -> acp::SessionNotification {
        let start = tokio::time::Instant::now();
        loop {
            if let Some(notification) = client.first_notification().await {
                return notification;
            }

            assert!(
                start.elapsed() < timeout,
                "timed out waiting for ACP session notification"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ws_transport_supports_acp_sdk_over_websocket_bridge() {
        let local_set = LocalSet::new();
        local_set
            .run_until(async {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let (stdin, stdout) = spawn_test_agent_streams();
                let server = tokio::spawn(async move {
                    run_stream_ws_with_listener(
                        stdin,
                        stdout,
                        "ws-test-agent",
                        BoundListener {
                            listener,
                            display_address: address.to_string(),
                        },
                    )
                    .await
                });

                let (outgoing, incoming, bridge) = connect_sdk_bridge(&format!("ws://{address}"))
                    .await
                    .unwrap();
                let client = RecordingClient::default();
                let (conn, io_task) =
                    acp::ClientSideConnection::new(client.clone(), outgoing, incoming, |fut| {
                        tokio::task::spawn_local(fut);
                    });
                tokio::task::spawn_local(io_task);

                let initialize = conn
                    .initialize(
                        acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_info(
                            acp::Implementation::new("ws-test-client", "0.1.0")
                                .title("WS Test Client"),
                        ),
                    )
                    .await
                    .unwrap();
                assert_eq!(initialize.protocol_version, acp::ProtocolVersion::V1);

                let session = conn
                    .new_session(acp::NewSessionRequest::new(
                        std::env::current_dir().unwrap(),
                    ))
                    .await
                    .unwrap();
                let prompt_result = conn
                    .prompt(acp::PromptRequest::new(
                        session.session_id.clone(),
                        vec!["ping".into()],
                    ))
                    .await
                    .unwrap();
                assert_eq!(prompt_result.stop_reason, acp::StopReason::EndTurn);

                let notification =
                    wait_for_session_notification(&client, Duration::from_secs(2)).await;
                match notification.update {
                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk {
                        content: acp::ContentBlock::Text(text),
                        ..
                    }) => assert_eq!(text.text, "hello over ws bridge"),
                    update => panic!("unexpected session update: {update:?}"),
                }

                bridge.shutdown().await.unwrap();
                tokio::time::timeout(Duration::from_secs(2), server)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            })
            .await;
    }
}
