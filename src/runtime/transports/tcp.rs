use std::convert::Infallible;
use std::future::pending;
use std::process::ExitStatus;

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncWriteExt, copy};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{ChildStdin, ChildStdout};

use crate::runtime::prepare::{CommandSpec, PreparedCommand};
use crate::runtime::process::{spawn_stream_child, terminate_child};

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

async fn serve_tcp_connection(
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

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::runtime::prepare::PreparedCommand;

    #[cfg(unix)]
    #[tokio::test]
    async fn tcp_transport_streams_raw_stdio_over_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            run_command_tcp_with_listener(
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

        let mut client = TcpStream::connect(address).await.unwrap();
        let mut first_chunk = [0_u8; 5];
        tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut first_chunk))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&first_chunk, b"boot\n");

        client.write_all(b"ping\n").await.unwrap();
        client.shutdown().await.unwrap();

        let mut echoed = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut echoed))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(echoed, b"ping\n");

        let status = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    async fn run_command_tcp_with_listener(
        prepared: PreparedCommand,
        subject: &str,
        bound_listener: BoundListener,
    ) -> Result<ExitStatus> {
        eprintln!(
            "Running {} over tcp://{} (raw stdio stream transport)",
            subject, bound_listener.display_address
        );

        let (socket, _) = bound_listener.listener.accept().await.with_context(|| {
            format!(
                "failed to accept TCP connection on {}",
                bound_listener.display_address
            )
        })?;

        serve_tcp_connection(prepared.spec, subject, socket).await
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
}
