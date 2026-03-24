use std::error::Error;
use std::fmt;
use std::io;
use std::process::{ExitStatus, Stdio};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, tcp::OwnedWriteHalf};
use tokio::process::{Child, Command};

use crate::install::{PreparedCommand, apply_command_spec};
use crate::registry::fetch_registry;
use crate::run::{RunError, prepare_run_command};

type BridgeTaskHandle = tokio::task::JoinHandle<Result<(), ServeError>>;
type BridgeTaskResult = Result<Result<(), ServeError>, tokio::task::JoinError>;

pub async fn serve_agent(
    agent_id: &str,
    host: &str,
    port: u16,
    user_args: &[String],
) -> Result<ExitStatus, ServeError> {
    let registry = fetch_registry()
        .await
        .map_err(RunError::FetchRegistry)
        .map_err(ServeError::Run)?;
    let agent = registry
        .get_agent(agent_id)
        .map_err(RunError::AgentNotFound)
        .map_err(ServeError::Run)?;

    let prepared = prepare_run_command(agent, user_args)
        .await
        .map_err(ServeError::Run)?;
    let bound_listener = bind_listener(host, port).await?;

    serve_single_connection(agent_id, bound_listener, prepared).await
}

struct BoundListener {
    listener: TcpListener,
    display_address: String,
}

async fn bind_listener(host: &str, port: u16) -> Result<BoundListener, ServeError> {
    let listener = TcpListener::bind((host, port))
        .await
        .map_err(|source| ServeError::Bind {
            address: bind_target_display(host, port),
            source,
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

async fn serve_single_connection(
    agent_id: &str,
    bound_listener: BoundListener,
    prepared: PreparedCommand,
) -> Result<ExitStatus, ServeError> {
    eprintln!(
        "Serving {} on tcp://{}",
        agent_id, bound_listener.display_address
    );
    let (socket, peer_address) =
        bound_listener
            .listener
            .accept()
            .await
            .map_err(|source| ServeError::Accept {
                address: bound_listener.display_address,
                source,
            })?;
    eprintln!("Accepted connection from {}", peer_address);

    serve_prepared_command(prepared, socket).await
}

async fn serve_prepared_command(
    prepared: PreparedCommand,
    socket: TcpStream,
) -> Result<ExitStatus, ServeError> {
    let _temp_dir = prepared.temp_dir;
    let mut command = Command::new(&prepared.spec.program);
    apply_command_spec(&mut command, &prepared.spec);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let program_display = prepared.spec.program.to_string_lossy().into_owned();
    let mut child = command.spawn().map_err(|source| ServeError::Spawn {
        program: program_display,
        source,
    })?;
    let mut child_stdin = child
        .stdin
        .take()
        .ok_or(ServeError::MissingChildPipe("stdin"))?;
    let child_stdout = child
        .stdout
        .take()
        .ok_or(ServeError::MissingChildPipe("stdout"))?;

    let (mut socket_reader, socket_writer) = socket.into_split();
    let mut stdout_handle = tokio::spawn(forward_reader_to_socket(child_stdout, socket_writer));
    let mut stdin_handle = tokio::spawn(async move {
        tokio::io::copy(&mut socket_reader, &mut child_stdin)
            .await
            .map_err(ServeError::BridgeIo)?;
        child_stdin.shutdown().await.map_err(ServeError::BridgeIo)
    });

    tokio::select! {
        status = child.wait() => {
            let status = status.map_err(ServeError::ChildWait)?;
            abort_task(stdin_handle).await;
            stdout_handle.await.map_err(ServeError::Join)??;
            Ok(status)
        }
        stdout_result = &mut stdout_handle => {
            handle_stdout_completion(stdout_result, &mut child, stdin_handle).await
        }
        stdin_result = &mut stdin_handle => {
            handle_stdin_completion(stdin_result, &mut child, stdout_handle).await
        }
    }
}

async fn handle_stdout_completion(
    stream_result: BridgeTaskResult,
    child: &mut Child,
    stdin_handle: BridgeTaskHandle,
) -> Result<ExitStatus, ServeError> {
    match stream_result.map_err(ServeError::Join)? {
        Ok(()) => {
            let status = wait_for_child(child).await?;
            abort_task(stdin_handle).await;
            Ok(status)
        }
        Err(error) => {
            abort_task_and_terminate_child(stdin_handle, child).await?;
            Err(error)
        }
    }
}

async fn handle_stdin_completion(
    stdin_result: BridgeTaskResult,
    child: &mut Child,
    stdout_handle: BridgeTaskHandle,
) -> Result<ExitStatus, ServeError> {
    match stdin_result {
        Ok(Ok(())) => {
            let status = wait_for_child(child).await?;
            stdout_handle.await.map_err(ServeError::Join)??;
            Ok(status)
        }
        Ok(Err(error)) => {
            abort_task_and_terminate_child(stdout_handle, child).await?;
            Err(error)
        }
        Err(join_error) => {
            abort_task_and_terminate_child(stdout_handle, child).await?;
            Err(ServeError::Join(join_error))
        }
    }
}

async fn forward_reader_to_socket<R>(
    mut reader: R,
    mut writer: OwnedWriteHalf,
) -> Result<(), ServeError>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(ServeError::BridgeIo)?;
        if read == 0 {
            return Ok(());
        }

        writer
            .write_all(&buffer[..read])
            .await
            .map_err(ServeError::BridgeIo)?;
        writer.flush().await.map_err(ServeError::BridgeIo)?;
    }
}

async fn terminate_child(child: &mut Child) -> Result<(), ServeError> {
    if child.try_wait().map_err(ServeError::ChildWait)?.is_none() {
        child.kill().await.map_err(ServeError::ChildWait)?;
        let _ = wait_for_child(child).await?;
    }

    Ok(())
}

async fn wait_for_child(child: &mut Child) -> Result<ExitStatus, ServeError> {
    child.wait().await.map_err(ServeError::ChildWait)
}

async fn abort_task_and_terminate_child(
    handle: BridgeTaskHandle,
    child: &mut Child,
) -> Result<(), ServeError> {
    abort_task(handle).await;
    terminate_child(child).await
}

async fn abort_task(handle: BridgeTaskHandle) {
    handle.abort();
    let _ = handle.await;
}

fn bind_target_display(host: &str, port: u16) -> String {
    format!("host={host}, port={port}")
}

#[derive(Debug)]
pub enum ServeError {
    Run(RunError),
    Bind { address: String, source: io::Error },
    Accept { address: String, source: io::Error },
    Spawn { program: String, source: io::Error },
    MissingChildPipe(&'static str),
    BridgeIo(io::Error),
    ChildWait(io::Error),
    Join(tokio::task::JoinError),
}

impl fmt::Display for ServeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Run(error) => write!(f, "{error}"),
            Self::Bind { address, source } => {
                write!(f, "Failed to bind TCP listener on {address}: {source}")
            }
            Self::Accept { address, source } => {
                write!(f, "Failed to accept TCP connection on {address}: {source}")
            }
            Self::Spawn { program, source } => {
                write!(f, "Failed to spawn {program}: {source}")
            }
            Self::MissingChildPipe(pipe) => write!(f, "Child process missing piped {pipe}"),
            Self::BridgeIo(source) => write!(f, "TCP bridge failed: {source}"),
            Self::ChildWait(source) => write!(f, "Failed while waiting on child process: {source}"),
            Self::Join(source) => write!(f, "Task join failed: {source}"),
        }
    }
}

impl Error for ServeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Bind { source, .. } => Some(source),
            Self::Accept { source, .. } => Some(source),
            Self::Spawn { source, .. } => Some(source),
            Self::Run(error) => Some(error),
            Self::BridgeIo(source) => Some(source),
            Self::ChildWait(source) => Some(source),
            Self::Join(source) => Some(source),
            Self::MissingChildPipe(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::install::{CommandSpec, PreparedCommand};

    #[cfg(unix)]
    #[tokio::test]
    async fn serves_one_connection_end_to_end_over_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            serve_single_connection(
                "demo-agent",
                BoundListener {
                    listener,
                    display_address: address.to_string(),
                },
                shell_prepared_command(
                    r#"printf 'boot-stderr\n' >&2
while IFS= read -r line; do
  printf 'stdout:%s\n' "$line"
  printf 'stderr:%s\n' "$line" >&2
done"#,
                ),
            )
            .await
        });

        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(b"alpha\nbeta\n").await.unwrap();
        client.shutdown().await.unwrap();

        let mut received = Vec::new();
        client.read_to_end(&mut received).await.unwrap();

        let status = server.await.unwrap().unwrap();
        assert!(status.success());

        let text = String::from_utf8(received).unwrap();
        assert!(text.contains("stdout:alpha"));
        assert!(text.contains("stdout:beta"));
        assert!(!text.contains("boot-stderr"));
        assert!(!text.contains("stderr:alpha"));
        assert!(!text.contains("stderr:beta"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn returns_child_exit_status_for_non_zero_process() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            serve_single_connection(
                "demo-agent",
                BoundListener {
                    listener,
                    display_address: address.to_string(),
                },
                shell_prepared_command("printf 'before-exit\\n'; exit 23"),
            )
            .await
        });

        let mut client = TcpStream::connect(address).await.unwrap();
        client.shutdown().await.unwrap();

        let mut received = Vec::new();
        client.read_to_end(&mut received).await.unwrap();

        let status = server.await.unwrap().unwrap();
        assert_eq!(status.code(), Some(23));
        assert_eq!(String::from_utf8(received).unwrap(), "before-exit\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn does_not_forward_stderr_to_tcp_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            serve_single_connection(
                "demo-agent",
                BoundListener {
                    listener,
                    display_address: address.to_string(),
                },
                shell_prepared_command("printf 'stderr-only\\n' >&2"),
            )
            .await
        });

        let mut client = TcpStream::connect(address).await.unwrap();
        client.shutdown().await.unwrap();

        let mut received = Vec::new();
        client.read_to_end(&mut received).await.unwrap();

        let status = server.await.unwrap().unwrap();
        assert!(status.success());
        assert!(received.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn returns_bridge_error_when_child_closes_stdin_early() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            serve_single_connection(
                "demo-agent",
                BoundListener {
                    listener,
                    display_address: address.to_string(),
                },
                shell_prepared_command("exec 0<&-; printf 'ready\\n'; sleep 5"),
            )
            .await
        });

        let mut client = TcpStream::connect(address).await.unwrap();
        let mut ready = [0_u8; 6];
        client.read_exact(&mut ready).await.unwrap();
        assert_eq!(&ready, b"ready\n");
        client.write_all(b"payload").await.unwrap();
        client.shutdown().await.unwrap();

        let completed = tokio::time::timeout(Duration::from_secs(1), server).await;
        assert!(completed.is_ok(), "server task did not finish promptly");

        let result = completed.unwrap().unwrap();
        assert!(matches!(result, Err(ServeError::BridgeIo(_))));
    }

    #[tokio::test]
    async fn bind_listener_accepts_ipv6_host_without_manual_formatting() {
        match bind_listener("::1", 0).await {
            Ok(BoundListener {
                listener,
                display_address: bind_address,
            }) => {
                let local = listener.local_addr().unwrap();
                assert!(local.is_ipv6());
                assert_eq!(bind_address, local.to_string());
            }
            Err(ServeError::Bind { source, .. })
                if matches!(
                    source.kind(),
                    io::ErrorKind::AddrNotAvailable | io::ErrorKind::Unsupported
                ) =>
            {
                // Some environments disable IPv6 entirely; skip instead of making this flaky.
            }
            Err(error) => panic!("unexpected bind result for IPv6 host: {error}"),
        }
    }

    #[test]
    fn run_error_preserves_source_chain() {
        let serve_error = ServeError::Run(RunError::Io(io::Error::other("boom")));

        let run_source = std::error::Error::source(&serve_error).expect("missing RunError source");
        assert_eq!(run_source.to_string(), "I/O failed: boom");

        let io_source = std::error::Error::source(run_source).expect("missing io::Error source");
        assert_eq!(io_source.to_string(), "boom");
    }

    #[cfg(unix)]
    fn shell_prepared_command(script: &str) -> PreparedCommand {
        PreparedCommand {
            spec: CommandSpec {
                program: OsString::from("sh"),
                args: vec![OsString::from("-c"), OsString::from(script)],
                env: Vec::new(),
                current_dir: None,
            },
            temp_dir: None,
        }
    }
}
