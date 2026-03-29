//! TCP transport that proxies stdin/stdout bytes without additional framing.
//! Only a single client is accepted so transports that need multiplexing should
//! use the WebSocket or HTTP/2 implementations instead.
use std::convert::Infallible;
use std::future::pending;
use std::process::ExitStatus;

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncWriteExt, copy};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{ChildStdin, ChildStdout};

use crate::runtime::prepare::{CommandSpec, PreparedCommand};
use crate::runtime::process::{spawn_stream_child, terminate_child};

/// Accepts one TCP client and shuffles raw bytes between the socket and the agent.
///
/// The transport keeps running until either the socket closes or the child process
/// exits, and it presents any IO errors via the `Result`.
pub async fn serve_tcp(
    prepared: PreparedCommand,
    subject: &str,
    host: &str,
    port: u16,
) -> Result<ExitStatus> {
    let bound_listener = bind_listener(host, port).await?;
    eprintln!(
        "Running {} over tcp://{} (raw stdio stream transport)",
        subject, bound_listener.display_address
    );

    let (socket, peer_address) = bound_listener.listener.accept().await.with_context(|| {
        format!(
            "failed to accept TCP connection on {}",
            bound_listener.display_address
        )
    })?;
    eprintln!("Accepted connection from {}", peer_address);

    let _temp_dir = prepared.temp_dir;
    serve_tcp_connection(prepared.spec, subject, socket).await
}

#[doc(hidden)]
pub async fn serve_tcp_connection(
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

    let (socket_reader, socket_writer) = socket.into_split();
    let stdin_pump = pump_socket_to_child_until_error(socket_reader, child_stdin);
    let stdout_pump = pump_child_to_socket(child_stdout, socket_writer);
    tokio::pin!(stdin_pump);
    tokio::pin!(stdout_pump);

    tokio::select! {
        status = child.wait() => status.context("failed while waiting on child process"),
        result = &mut stdin_pump => {
            match result {
                Ok(never) => match never {},
                Err(source) => {
                    let _ = terminate_child(&mut child).await;
                    Err(anyhow!("TCP transport failed: {source}"))
                }
            }
        }
        result = &mut stdout_pump => {
            match result {
                Ok(()) => terminate_child(&mut child).await,
                Err(source) => {
                    let _ = terminate_child(&mut child).await;
                    Err(anyhow!("TCP transport failed: {source}"))
                }
            }
        }
    }
}

async fn pump_socket_to_child_until_error(
    mut socket_reader: tokio::net::tcp::OwnedReadHalf,
    mut child_stdin: ChildStdin,
) -> Result<Infallible> {
    copy(&mut socket_reader, &mut child_stdin)
        .await
        .context("failed to read from TCP client or write to child stdin")?;
    child_stdin
        .shutdown()
        .await
        .context("failed to close child stdin after TCP client EOF")?;
    drop(child_stdin);

    Ok(pending::<Infallible>().await)
}

async fn pump_child_to_socket(
    mut child_stdout: ChildStdout,
    mut socket_writer: tokio::net::tcp::OwnedWriteHalf,
) -> Result<()> {
    copy(&mut child_stdout, &mut socket_writer)
        .await
        .context("failed to read from child stdout or write to TCP client")?;
    socket_writer
        .shutdown()
        .await
        .context("failed to close TCP socket after child stdout EOF")?;
    Ok(())
}

struct BoundListener {
    listener: TcpListener,
    display_address: String,
}

async fn bind_listener(host: &str, port: u16) -> Result<BoundListener> {
    let listener = TcpListener::bind((host, port)).await.with_context(|| {
        format!(
            "failed to bind TCP listener on {}",
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
