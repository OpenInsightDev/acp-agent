use std::error::Error;
use std::fmt;
use std::io;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, tcp::OwnedWriteHalf};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::install::{PreparedCommand, apply_command_spec};
use crate::registry::fetch_registry;
use crate::run::{RunError, prepare_run_command};

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
    let bind_address = format!("{host}:{port}");
    let listener = TcpListener::bind(&bind_address)
        .await
        .map_err(|source| ServeError::Bind {
            address: bind_address.clone(),
            source,
        })?;

    eprintln!("Serving {} on tcp://{}", agent.id, bind_address);
    let (socket, peer_address) = listener
        .accept()
        .await
        .map_err(|source| ServeError::Accept {
            address: bind_address,
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
        .stderr(Stdio::piped());

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
    let child_stderr = child
        .stderr
        .take()
        .ok_or(ServeError::MissingChildPipe("stderr"))?;

    let (mut socket_reader, socket_writer) = socket.into_split();
    let shared_writer = Arc::new(Mutex::new(socket_writer));
    let mut stdout_handle = tokio::spawn(forward_reader_to_socket(
        child_stdout,
        Arc::clone(&shared_writer),
    ));
    let mut stderr_handle = tokio::spawn(forward_reader_to_socket(
        child_stderr,
        Arc::clone(&shared_writer),
    ));
    let stdin_handle = tokio::spawn(async move {
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
            stderr_handle.await.map_err(ServeError::Join)??;
            Ok(status)
        }
        stdout_result = &mut stdout_handle => {
            handle_stream_completion(stdout_result, &mut child, stdin_handle, stderr_handle).await
        }
        stderr_result = &mut stderr_handle => {
            handle_stream_completion(stderr_result, &mut child, stdin_handle, stdout_handle).await
        }
    }
}

async fn handle_stream_completion(
    stream_result: Result<Result<(), ServeError>, tokio::task::JoinError>,
    child: &mut Child,
    stdin_handle: tokio::task::JoinHandle<Result<(), ServeError>>,
    sibling_handle: tokio::task::JoinHandle<Result<(), ServeError>>,
) -> Result<ExitStatus, ServeError> {
    match stream_result.map_err(ServeError::Join)? {
        Ok(()) => {
            let status = child.wait().await.map_err(ServeError::ChildWait)?;
            abort_task(stdin_handle).await;
            sibling_handle.await.map_err(ServeError::Join)??;
            Ok(status)
        }
        Err(error) => {
            abort_task(stdin_handle).await;
            abort_task(sibling_handle).await;
            terminate_child(child).await?;
            Err(error)
        }
    }
}

async fn forward_reader_to_socket<R>(
    mut reader: R,
    writer: Arc<Mutex<OwnedWriteHalf>>,
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

        let mut writer = writer.lock().await;
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
        let _ = child.wait().await.map_err(ServeError::ChildWait)?;
    }

    Ok(())
}

async fn abort_task(handle: tokio::task::JoinHandle<Result<(), ServeError>>) {
    handle.abort();
    let _ = handle.await;
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
            Self::BridgeIo(source) => Some(source),
            Self::ChildWait(source) => Some(source),
            Self::Join(source) => Some(source),
            Self::Run(_) | Self::MissingChildPipe(_) => None,
        }
    }
}
