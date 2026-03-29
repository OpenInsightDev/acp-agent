//! WebSocket transport: one text frame = one ACP JSON-RPC message.
//!
//! Inbound (ws → child stdin): each `Text` frame must be a well-formed JSON
//! object without embedded newlines.  The frame text is forwarded to the child
//! followed by `\n` (NDJSON framing).
//!
//! Outbound (child stdout → ws): the child's NDJSON output is split on `\n`
//! and each complete non-empty line is sent as a single `Text` frame without
//! the trailing newline.
use std::process::ExitStatus;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{ChildStdin, ChildStdout};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use crate::runtime::prepare::{CommandSpec, PreparedCommand};
use crate::runtime::process::{spawn_stream_child, terminate_child};

/// Grace window before force-killing child after WebSocket closes.
const STDIN_CLOSE_GRACE: Duration = Duration::from_secs(5);
const READ_BUFFER_SIZE: usize = 8192;

/// Accepts one WebSocket connection and exposes the agent via ACP message framing.
///
/// One WebSocket `Text` frame maps to exactly one ACP JSON-RPC message. The
/// child process communicates over NDJSON on its stdio; newline translation
/// occurs only at the transport boundary.
pub async fn serve_ws(
    prepared: PreparedCommand,
    subject: &str,
    host: &str,
    port: u16,
) -> Result<ExitStatus> {
    let bound_listener = bind_listener(host, port).await?;
    eprintln!(
        "Running {} over ws://{} (ACP message per WebSocket frame)",
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

    let ws = accept_ws_connection(socket).await?;
    let (ws_sink, ws_stream) = ws.split();

    let ws_to_stdin = pump_ws_to_child_stdin(ws_stream, child_stdin);
    let stdout_to_ws = pump_child_stdout_to_ws(child_stdout, ws_sink);
    tokio::pin!(ws_to_stdin);
    tokio::pin!(stdout_to_ws);

    tokio::select! {
        // WS side closed or errored.
        result = &mut ws_to_stdin => {
            match result {
                Ok(mut child_stdin) => {
                    // WebSocket closed normally: close stdin then wait for child to finish.
                    let _ = child_stdin.shutdown().await;
                    drop(child_stdin);
                    // Wait for the stdout pump to finish processing remaining output.
                    let stdout_result = tokio::time::timeout(STDIN_CLOSE_GRACE, &mut stdout_to_ws).await;
                    match stdout_result {
                        Ok(Ok(())) => {}
                        Ok(Err(source)) => {
                            let _ = terminate_child(&mut child).await;
                            return Err(anyhow!("WebSocket transport failed: {source}"));
                        }
                        Err(_timeout) => {
                            // stdout pump timed out — force-kill.
                            let _ = terminate_child(&mut child).await;
                        }
                    }
                    let status = tokio::time::timeout(STDIN_CLOSE_GRACE, child.wait()).await;
                    match status {
                        Ok(Ok(s)) => Ok(s),
                        _ => terminate_child(&mut child).await,
                    }
                }
                Err(source) => {
                    let _ = terminate_child(&mut child).await;
                    Err(anyhow!("WebSocket transport failed: {source}"))
                }
            }
        }
        // Child stdout ended or errored — stdout pump drives completion.
        result = &mut stdout_to_ws => {
            match result {
                Ok(()) => {
                    // Stdout closed cleanly (all lines were newline-terminated).
                    terminate_child(&mut child).await
                }
                Err(source) => {
                    let _ = terminate_child(&mut child).await;
                    Err(anyhow!("WebSocket transport failed: {source}"))
                }
            }
        }
    }
}

async fn accept_ws_connection(socket: TcpStream) -> Result<WebSocketStream<TcpStream>> {
    tokio_tungstenite::accept_async(socket)
        .await
        .context("WebSocket handshake failed")
}

/// Reads WebSocket `Text` frames, validates them, and writes to child stdin as NDJSON.
///
/// Returns the `ChildStdin` on normal WebSocket close so the caller can gracefully
/// shut stdin before waiting on the child.  Returns an error on protocol violations.
async fn pump_ws_to_child_stdin(
    mut ws_stream: impl futures_util::Stream<
        Item = Result<Message, tokio_tungstenite::tungstenite::Error>,
    > + Unpin,
    mut child_stdin: ChildStdin,
) -> Result<ChildStdin> {
    while let Some(msg) = ws_stream.next().await {
        let msg = msg.context("WebSocket read error")?;
        match msg {
            Message::Text(text) => {
                validate_inbound_ws_message(&text)?;
                child_stdin
                    .write_all(text.as_bytes())
                    .await
                    .context("failed to write to child stdin")?;
                child_stdin
                    .write_all(b"\n")
                    .await
                    .context("failed to write newline to child stdin")?;
            }
            Message::Binary(_) => {
                return Err(anyhow!(
                    "WebSocket transport only accepts Text frames; Binary frame rejected"
                ));
            }
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }
    Ok(child_stdin)
}

/// Validates that a single WebSocket text frame is a legal ACP message.
fn validate_inbound_ws_message(text: &str) -> Result<()> {
    if text.contains('\n') || text.contains('\r') {
        return Err(anyhow!(
            "WebSocket frame must not contain newlines (would corrupt NDJSON framing)"
        ));
    }
    let value: serde_json::Value =
        serde_json::from_str(text).context("WebSocket frame is not valid JSON")?;
    if !value.is_object() {
        return Err(anyhow!(
            "WebSocket frame JSON must be an object (got {})",
            match &value {
                serde_json::Value::Array(_) => "array",
                serde_json::Value::Bool(_) => "bool",
                serde_json::Value::Null => "null",
                serde_json::Value::Number(_) => "number",
                serde_json::Value::String(_) => "string",
                serde_json::Value::Object(_) => "object",
            }
        ));
    }
    Ok(())
}

/// Reads child stdout as UTF-8 NDJSON and sends each complete line as a `Text` frame.
async fn pump_child_stdout_to_ws(
    mut child_stdout: ChildStdout,
    mut ws_sink: impl futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
) -> Result<()> {
    let mut raw_buf = vec![0u8; READ_BUFFER_SIZE];
    // Accumulate undecoded bytes for incremental UTF-8 decoding.
    let mut byte_buf: Vec<u8> = Vec::new();
    // Characters decoded and awaiting `\n`.
    let mut line_buf = String::new();

    loop {
        let n = child_stdout
            .read(&mut raw_buf)
            .await
            .context("failed to read from child stdout")?;

        if n == 0 {
            // EOF: any remaining data in line_buf is an unterminated line.
            if !line_buf.is_empty() {
                return Err(anyhow!(
                    "child stdout closed with unterminated line (missing trailing newline)"
                ));
            }
            // Close the WebSocket gracefully.
            let _ = ws_sink.close().await;
            return Ok(());
        }

        byte_buf.extend_from_slice(&raw_buf[..n]);

        // Incrementally decode UTF-8, leaving incomplete multibyte sequences in byte_buf.
        let text = match std::str::from_utf8(&byte_buf) {
            Ok(s) => {
                let owned = s.to_owned();
                byte_buf.clear();
                owned
            }
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                if e.error_len().is_some() {
                    // Invalid UTF-8 sequence (not just incomplete).
                    return Err(anyhow!("child stdout contains invalid UTF-8 data"));
                }
                // Incomplete multibyte sequence at end — keep those bytes for next read.
                let owned = std::str::from_utf8(&byte_buf[..valid_up_to])
                    .unwrap()
                    .to_owned();
                byte_buf.drain(..valid_up_to);
                owned
            }
        };

        line_buf.push_str(&text);

        // Split completed lines and send each as a Text frame.
        while let Some(newline_pos) = line_buf.find('\n') {
            let line: String = line_buf.drain(..newline_pos).collect();
            // Remove the `\n` itself.
            line_buf.remove(0);

            if !line.is_empty() {
                ws_sink
                    .send(Message::Text(line.into()))
                    .await
                    .context("failed to send WebSocket frame")?;
            }
        }
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

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    use super::*;
    use crate::runtime::prepare::{CommandSpec, PreparedCommand};

    // ── helpers ────────────────────────────────────────────────────────────────

    #[cfg(unix)]
    fn prepared_command(program: &str, args: &[&str]) -> PreparedCommand {
        PreparedCommand {
            spec: CommandSpec {
                program: OsString::from(program),
                args: args.iter().map(|s| OsString::from(*s)).collect(),
                env: Vec::new(),
                current_dir: None,
            },
            temp_dir: None,
        }
    }

    #[cfg(unix)]
    async fn setup(
        program: &str,
        args: &[&str],
    ) -> (
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
        tokio::task::JoinHandle<Result<ExitStatus>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let prepared = prepared_command(program, args);

        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            serve_ws_connection(prepared.spec, "test-agent", socket).await
        });

        let (ws, _) = connect_async(format!("ws://{address}")).await.unwrap();
        (ws, server)
    }

    // ── framing happy path ─────────────────────────────────────────────────────

    #[cfg(unix)]
    #[tokio::test]
    async fn ws_transport_single_frame_becomes_ndjson_line() {
        let (mut ws, server) = setup("sh", &["-c", "cat"]).await;

        ws.send(Message::Text(r#"{"jsonrpc":"2.0","method":"ping"}"#.into()))
            .await
            .unwrap();

        let reply = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            reply,
            Message::Text(r#"{"jsonrpc":"2.0","method":"ping"}"#.into())
        );

        ws.close(None).await.unwrap();
        let status = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ws_transport_multiple_ndjson_lines_become_multiple_frames() {
        let (mut ws, server) = setup("sh", &["-c", r#"printf '{"id":1}\n{"id":2}\n'"#]).await;

        let f1 = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let f2 = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(f1, Message::Text(r#"{"id":1}"#.into()));
        assert_eq!(f2, Message::Text(r#"{"id":2}"#.into()));

        let _ = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ws_transport_empty_lines_are_not_sent_as_frames() {
        let (mut ws, server) = setup("sh", &["-c", r#"printf '{"id":1}\n\n{"id":2}\n'"#]).await;

        let f1 = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let f2 = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(f1, Message::Text(r#"{"id":1}"#.into()));
        assert_eq!(f2, Message::Text(r#"{"id":2}"#.into()));

        let _ = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    // ── inbound validation ─────────────────────────────────────────────────────

    #[cfg(unix)]
    #[tokio::test]
    async fn ws_transport_rejects_binary_frame() {
        let (mut ws, server) = setup("sh", &["-c", "cat"]).await;

        ws.send(Message::Binary(b"data".to_vec().into()))
            .await
            .unwrap();

        let result = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ws_transport_rejects_frame_with_newline() {
        let (mut ws, server) = setup("sh", &["-c", "cat"]).await;

        ws.send(Message::Text("{\"a\":1}\n{\"b\":2}".into()))
            .await
            .unwrap();

        let result = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ws_transport_rejects_frame_with_carriage_return() {
        let (mut ws, server) = setup("sh", &["-c", "cat"]).await;

        ws.send(Message::Text("{\"a\":1}\r".into())).await.unwrap();

        let result = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ws_transport_rejects_non_json_frame() {
        let (mut ws, server) = setup("sh", &["-c", "cat"]).await;

        ws.send(Message::Text("not json".into())).await.unwrap();

        let result = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ws_transport_rejects_non_object_json() {
        let (mut ws, server) = setup("sh", &["-c", "cat"]).await;

        ws.send(Message::Text("[1,2,3]".into())).await.unwrap();

        let result = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err());
    }

    // ── outbound failure paths ─────────────────────────────────────────────────

    #[cfg(unix)]
    #[tokio::test]
    async fn ws_transport_fails_on_unterminated_stdout_line() {
        // Script writes data without a trailing newline then exits.
        let (_ws, server) = setup("sh", &["-c", r#"printf '{"id":1}'"#]).await;

        let result = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        assert!(
            result.is_err(),
            "expected error for unterminated stdout line"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ws_transport_closes_ws_cleanly_when_child_exits_with_newline() {
        let (mut ws, server) = setup("sh", &["-c", r#"printf '{"ok":true}\n'"#]).await;

        let frame = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(frame, Message::Text(r#"{"ok":true}"#.into()));

        let next = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .unwrap();
        if let Some(Ok(msg)) = next {
            assert!(matches!(msg, Message::Close(_)));
        }

        let status = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(status.success());
    }

    // ── validate_inbound_ws_message unit tests ─────────────────────────────────

    #[test]
    fn validate_accepts_valid_object() {
        assert!(validate_inbound_ws_message(r#"{"method":"ping"}"#).is_ok());
    }

    #[test]
    fn validate_rejects_newline() {
        assert!(validate_inbound_ws_message("{\"a\":1}\n").is_err());
    }

    #[test]
    fn validate_rejects_carriage_return() {
        assert!(validate_inbound_ws_message("{\"a\":1}\r").is_err());
    }

    #[test]
    fn validate_rejects_non_json() {
        assert!(validate_inbound_ws_message("hello").is_err());
    }

    #[test]
    fn validate_rejects_json_array() {
        assert!(validate_inbound_ws_message("[1,2]").is_err());
    }

    #[test]
    fn validate_rejects_json_null() {
        assert!(validate_inbound_ws_message("null").is_err());
    }

    // ── ACP SDK end-to-end tests ───────────────────────────────────────────────
    //
    // These tests run a real `AgentSideConnection` in-process over the WS
    // transport to verify the full ACP protocol flow with the new framing.

    #[cfg(unix)]
    mod acp_e2e {
        use std::cell::Cell;
        use std::io;
        use std::pin::Pin;
        use std::rc::Rc;
        use std::task::{Context as Ctx, Poll};

        use agent_client_protocol::{self as acp, Agent as _, Client as _};
        use futures_util::{SinkExt, StreamExt};
        use tokio::net::TcpListener;
        use tokio::sync::{mpsc, oneshot};
        use tokio::task::LocalSet;
        use tokio_tungstenite::connect_async;
        use tokio_tungstenite::tungstenite::Message;

        // ── In-process test agent ──────────────────────────────────────────────

        struct EchoAgent {
            notification_tx: mpsc::UnboundedSender<(acp::SessionNotification, oneshot::Sender<()>)>,
            next_session_id: Cell<u64>,
        }

        impl EchoAgent {
            fn new(
                tx: mpsc::UnboundedSender<(acp::SessionNotification, oneshot::Sender<()>)>,
            ) -> Self {
                Self {
                    notification_tx: tx,
                    next_session_id: Cell::new(0),
                }
            }
        }

        #[async_trait::async_trait(?Send)]
        impl acp::Agent for EchoAgent {
            async fn initialize(
                &self,
                args: acp::InitializeRequest,
            ) -> acp::Result<acp::InitializeResponse> {
                Ok(
                    acp::InitializeResponse::new(args.protocol_version).agent_info(
                        acp::Implementation::new("ws-e2e-agent", "0.1.0").title("WS E2E Agent"),
                    ),
                )
            }

            async fn authenticate(
                &self,
                _args: acp::AuthenticateRequest,
            ) -> acp::Result<acp::AuthenticateResponse> {
                Ok(acp::AuthenticateResponse::default())
            }

            async fn new_session(
                &self,
                _args: acp::NewSessionRequest,
            ) -> acp::Result<acp::NewSessionResponse> {
                let id = self.next_session_id.get();
                self.next_session_id.set(id + 1);
                Ok(acp::NewSessionResponse::new(id.to_string()))
            }

            async fn load_session(
                &self,
                _args: acp::LoadSessionRequest,
            ) -> acp::Result<acp::LoadSessionResponse> {
                Ok(acp::LoadSessionResponse::new())
            }

            async fn set_session_mode(
                &self,
                _args: acp::SetSessionModeRequest,
            ) -> acp::Result<acp::SetSessionModeResponse> {
                Ok(acp::SetSessionModeResponse::new())
            }

            async fn prompt(&self, args: acp::PromptRequest) -> acp::Result<acp::PromptResponse> {
                let (ack_tx, ack_rx) = oneshot::channel();
                self.notification_tx
                    .send((
                        acp::SessionNotification::new(
                            args.session_id,
                            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                                "pong".into(),
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

        // ── Minimal no-op client ───────────────────────────────────────────────

        #[derive(Clone, Default)]
        struct NullClient {
            notifications: std::sync::Arc<std::sync::Mutex<Vec<acp::SessionNotification>>>,
        }

        #[async_trait::async_trait(?Send)]
        impl acp::Client for NullClient {
            async fn request_permission(
                &self,
                _a: acp::RequestPermissionRequest,
            ) -> acp::Result<acp::RequestPermissionResponse> {
                Err(acp::Error::method_not_found())
            }
            async fn write_text_file(
                &self,
                _a: acp::WriteTextFileRequest,
            ) -> acp::Result<acp::WriteTextFileResponse> {
                Err(acp::Error::method_not_found())
            }
            async fn read_text_file(
                &self,
                _a: acp::ReadTextFileRequest,
            ) -> acp::Result<acp::ReadTextFileResponse> {
                Err(acp::Error::method_not_found())
            }
            async fn create_terminal(
                &self,
                _a: acp::CreateTerminalRequest,
            ) -> acp::Result<acp::CreateTerminalResponse> {
                Err(acp::Error::method_not_found())
            }
            async fn terminal_output(
                &self,
                _a: acp::TerminalOutputRequest,
            ) -> acp::Result<acp::TerminalOutputResponse> {
                Err(acp::Error::method_not_found())
            }
            async fn release_terminal(
                &self,
                _a: acp::ReleaseTerminalRequest,
            ) -> acp::Result<acp::ReleaseTerminalResponse> {
                Err(acp::Error::method_not_found())
            }
            async fn wait_for_terminal_exit(
                &self,
                _a: acp::WaitForTerminalExitRequest,
            ) -> acp::Result<acp::WaitForTerminalExitResponse> {
                Err(acp::Error::method_not_found())
            }
            async fn kill_terminal(
                &self,
                _a: acp::KillTerminalRequest,
            ) -> acp::Result<acp::KillTerminalResponse> {
                Err(acp::Error::method_not_found())
            }
            async fn session_notification(&self, n: acp::SessionNotification) -> acp::Result<()> {
                self.notifications.lock().unwrap().push(n);
                Ok(())
            }
            async fn ext_method(&self, _a: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
                Err(acp::Error::method_not_found())
            }
            async fn ext_notification(&self, _a: acp::ExtNotification) -> acp::Result<()> {
                Err(acp::Error::method_not_found())
            }
        }

        // ── WS ↔ futures::AsyncRead/Write adapter ─────────────────────────────
        //
        // The ACP SDK expects `futures::AsyncRead + AsyncWrite` (from the
        // `futures` crate), but tokio-tungstenite uses its own `SplitSink` /
        // `SplitStream`.  We bridge them with a pair of `piper::pipe` channels:
        //
        //   WsWriter  – receives bytes from the ACP encoder, accumulates them
        //               into complete NDJSON lines, and forwards each line as a
        //               single WS Text frame (stripped of the trailing `\n`).
        //
        //   WsReader  – receives WS Text frames, appends `\n`, and exposes them
        //               as a byte stream to the ACP decoder.

        /// An `AsyncWrite` that converts NDJSON byte writes into WS Text frames.
        struct WsWriter {
            /// Byte buffer – accumulates data until a `\n` is seen.
            buf: Vec<u8>,
            /// Channel to the background pump task.
            frame_tx: mpsc::UnboundedSender<String>,
        }

        impl futures_util::io::AsyncWrite for WsWriter {
            fn poll_write(
                mut self: Pin<&mut Self>,
                _cx: &mut Ctx<'_>,
                data: &[u8],
            ) -> Poll<io::Result<usize>> {
                self.buf.extend_from_slice(data);
                while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                    let line_bytes: Vec<u8> = self.buf.drain(..=pos).collect();
                    let text = String::from_utf8(line_bytes[..line_bytes.len() - 1].to_vec())
                        .map_err(|_| {
                            io::Error::new(io::ErrorKind::InvalidData, "non-UTF8 message")
                        })?;
                    self.frame_tx.send(text).map_err(|_| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "ws writer closed")
                    })?;
                }
                Poll::Ready(Ok(data.len()))
            }

            fn poll_flush(self: Pin<&mut Self>, _cx: &mut Ctx<'_>) -> Poll<io::Result<()>> {
                Poll::Ready(Ok(()))
            }

            fn poll_close(self: Pin<&mut Self>, _cx: &mut Ctx<'_>) -> Poll<io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }

        /// An `AsyncRead` that turns WS Text frames into a byte stream.
        struct WsReader {
            /// Incoming frame bytes (with appended `\n`) not yet consumed.
            leftover: Vec<u8>,
            offset: usize,
            /// Channel from the background pump task.
            frame_rx: mpsc::UnboundedReceiver<Vec<u8>>,
        }

        impl futures_util::io::AsyncRead for WsReader {
            fn poll_read(
                mut self: Pin<&mut Self>,
                cx: &mut Ctx<'_>,
                buf: &mut [u8],
            ) -> Poll<io::Result<usize>> {
                if self.offset < self.leftover.len() {
                    let remaining = &self.leftover[self.offset..];
                    let n = remaining.len().min(buf.len());
                    buf[..n].copy_from_slice(&remaining[..n]);
                    self.offset += n;
                    if self.offset == self.leftover.len() {
                        self.leftover.clear();
                        self.offset = 0;
                    }
                    return Poll::Ready(Ok(n));
                }

                match self.frame_rx.poll_recv(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(None) => Poll::Ready(Ok(0)),
                    Poll::Ready(Some(bytes)) => {
                        let n = bytes.len().min(buf.len());
                        buf[..n].copy_from_slice(&bytes[..n]);
                        if n < bytes.len() {
                            self.leftover = bytes[n..].to_vec();
                            self.offset = 0;
                        }
                        Poll::Ready(Ok(n))
                    }
                }
            }
        }

        // ── Helper: build a WsWriter+WsReader pair from a WS connection ───────

        fn ws_to_acp_io<S>(ws: tokio_tungstenite::WebSocketStream<S>) -> (WsWriter, WsReader)
        where
            S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + 'static,
        {
            let (mut ws_sink, mut ws_stream) = ws.split();
            let (frame_tx, mut outgoing_rx) = mpsc::unbounded_channel::<String>();
            let (incoming_tx, frame_rx) = mpsc::unbounded_channel::<Vec<u8>>();

            // Pump outgoing frames.
            tokio::task::spawn_local(async move {
                while let Some(text) = outgoing_rx.recv().await {
                    if ws_sink.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                let _ = ws_sink.close().await;
            });

            // Pump incoming frames.
            tokio::task::spawn_local(async move {
                while let Some(msg) = ws_stream.next().await {
                    match msg {
                        Ok(Message::Text(text)) => {
                            let mut bytes = text.as_bytes().to_vec();
                            bytes.push(b'\n');
                            if incoming_tx.send(bytes).is_err() {
                                break;
                            }
                        }
                        Ok(Message::Close(_)) | Err(_) => break,
                        _ => {}
                    }
                }
            });

            (
                WsWriter {
                    buf: Vec::new(),
                    frame_tx,
                },
                WsReader {
                    leftover: Vec::new(),
                    offset: 0,
                    frame_rx,
                },
            )
        }

        // ── Test setup helper ─────────────────────────────────────────────────

        async fn setup_acp_ws() -> (acp::ClientSideConnection, NullClient) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();

            // Server side: accept one WS connection, wire to AgentSideConnection.
            tokio::task::spawn_local(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                let (writer, reader) = ws_to_acp_io(ws);

                let (notification_tx, mut notification_rx) = mpsc::unbounded_channel();
                let agent = EchoAgent::new(notification_tx);

                let (conn, io_task) = acp::AgentSideConnection::new(agent, writer, reader, |fut| {
                    tokio::task::spawn_local(fut);
                });
                let conn = Rc::new(conn);
                let notify_conn = Rc::clone(&conn);

                tokio::task::spawn_local(io_task);
                tokio::task::spawn_local(async move {
                    while let Some((notif, ack)) = notification_rx.recv().await {
                        if notify_conn.session_notification(notif).await.is_err() {
                            break;
                        }
                        let _ = ack.send(());
                    }
                });
            });

            // Client side.
            let (ws, _) = connect_async(format!("ws://{address}")).await.unwrap();
            let (writer, reader) = ws_to_acp_io(ws);

            let null_client = NullClient::default();
            let (conn, io_task) =
                acp::ClientSideConnection::new(null_client.clone(), writer, reader, |fut| {
                    tokio::task::spawn_local(fut);
                });
            tokio::task::spawn_local(io_task);

            (conn, null_client)
        }

        // ── Tests ─────────────────────────────────────────────────────────────

        #[tokio::test(flavor = "current_thread")]
        async fn acp_initialize_over_ws_transport() {
            LocalSet::new()
                .run_until(async {
                    let (conn, _client) = setup_acp_ws().await;

                    let resp = conn
                        .initialize(
                            acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_info(
                                acp::Implementation::new("test-client", "0.1.0")
                                    .title("Test Client"),
                            ),
                        )
                        .await
                        .expect("initialize failed");

                    assert_eq!(resp.protocol_version, acp::ProtocolVersion::V1);
                    let info = resp.agent_info.unwrap();
                    assert_eq!(info.name, "ws-e2e-agent");
                })
                .await;
        }

        #[tokio::test(flavor = "current_thread")]
        async fn acp_new_session_over_ws_transport() {
            LocalSet::new()
                .run_until(async {
                    let (conn, _client) = setup_acp_ws().await;

                    let resp = conn
                        .new_session(acp::NewSessionRequest::new(
                            std::env::current_dir().unwrap(),
                        ))
                        .await
                        .expect("new_session failed");

                    assert_eq!(resp.session_id.0.as_ref(), "0");
                })
                .await;
        }

        #[tokio::test(flavor = "current_thread")]
        async fn acp_prompt_and_session_notification_over_ws_transport() {
            LocalSet::new()
                .run_until(async {
                    let (conn, client) = setup_acp_ws().await;

                    let session = conn
                        .new_session(acp::NewSessionRequest::new(
                            std::env::current_dir().unwrap(),
                        ))
                        .await
                        .unwrap();

                    let prompt_resp = conn
                        .prompt(acp::PromptRequest::new(
                            session.session_id,
                            vec!["ping".into()],
                        ))
                        .await
                        .expect("prompt failed");

                    assert_eq!(prompt_resp.stop_reason, acp::StopReason::EndTurn);

                    // Yield so notification delivery completes.
                    for _ in 0..20 {
                        tokio::task::yield_now().await;
                    }

                    let notifs = client.notifications.lock().unwrap();
                    assert_eq!(notifs.len(), 1);
                    match &notifs[0].update {
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk {
                            content: acp::ContentBlock::Text(t),
                            ..
                        }) => assert_eq!(t.text, "pong"),
                        other => panic!("unexpected update: {other:?}"),
                    }
                })
                .await;
        }
    }
}
