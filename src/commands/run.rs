use std::convert::Infallible;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::header::CONTENT_TYPE;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Version};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio_stream::wrappers::ReceiverStream;

use super::install::{
    InstallError, download_archive, extract_archive, make_executable, resolve_cmd_path,
};
use crate::registry::{BinaryTarget, Environment, Platform, RegistryAgent, fetch_registry};

const H2_STREAM_CONTENT_TYPE: &str = "application/octet-stream";
const STREAM_BUFFER_CAPACITY: usize = 16;
const COPY_BUFFER_SIZE: usize = 8192;

type ResponseBody = BoxBody<Bytes, Infallible>;
type ResponseFrame = Result<Frame<Bytes>, Infallible>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RunTransport {
    Stdio,
    Http,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOptions {
    pub transport: RunTransport,
    pub host: String,
    pub port: u16,
}

pub async fn run_agent(
    agent_id: &str,
    options: RunOptions,
    user_args: &[String],
) -> Result<ExitStatus> {
    let registry = fetch_registry().await?;
    let agent = registry
        .get_agent(agent_id)
        .with_context(|| format!("failed to resolve agent \"{agent_id}\" from registry"))?;

    let prepared = prepare_run_command(agent, user_args).await?;
    run_prepared_command(prepared, options, &agent.id).await
}

async fn prepare_run_command(
    agent: &RegistryAgent,
    user_args: &[String],
) -> Result<PreparedCommand> {
    if let Some(binary) = &agent.distribution.binary {
        let platform = Platform::current()?;
        if let Some(target) = binary.for_platform(platform) {
            let temp_dir = tokio::task::spawn_blocking(tempfile::tempdir)
                .await
                .context("failed to create temporary directory task")?
                .context("failed to create temporary directory")?;
            let archive_path = download_archive(target, temp_dir.path())
                .await
                .map_err(run_error_from_install)?;
            let extracted_dir = temp_dir.path().join("extracted");
            tokio::fs::create_dir_all(&extracted_dir)
                .await
                .with_context(|| format!("failed to create {}", extracted_dir.display()))?;
            extract_archive(archive_path, extracted_dir.clone())
                .await
                .map_err(run_error_from_install)?;

            let executable_path = resolve_cmd_path(&extracted_dir, &target.cmd);
            let metadata = tokio::fs::metadata(&executable_path).await;
            if metadata
                .as_ref()
                .map(|metadata| !metadata.is_file())
                .unwrap_or(true)
            {
                bail!(
                    "downloaded {}, but could not find \"{}\" at {}",
                    target.archive,
                    target.cmd,
                    executable_path.display()
                );
            }

            make_executable(&executable_path)
                .await
                .with_context(|| format!("failed to mark {} executable", executable_path.display()))?;

            return Ok(PreparedCommand {
                spec: binary_command_spec(executable_path, extracted_dir, target, user_args),
                temp_dir: Some(temp_dir),
            });
        }
    }

    if let Some(npx) = &agent.distribution.npx {
        return Ok(PreparedCommand {
            spec: package_command_spec(
                "npx",
                &npx.package,
                npx.args.as_ref(),
                npx.env.as_ref(),
                user_args,
            ),
            temp_dir: None,
        });
    }

    if let Some(uvx) = &agent.distribution.uvx {
        return Ok(PreparedCommand {
            spec: package_command_spec(
                "uvx",
                &uvx.package,
                uvx.args.as_ref(),
                uvx.env.as_ref(),
                user_args,
            ),
            temp_dir: None,
        });
    }

    bail!(
        "agent \"{}\" does not have a runnable distribution",
        agent.id
    )
}

async fn run_prepared_command(
    prepared: PreparedCommand,
    options: RunOptions,
    subject: &str,
) -> Result<ExitStatus> {
    match options.transport {
        RunTransport::Stdio => {
            let _temp_dir = prepared.temp_dir;
            run_command_stdio(prepared.spec, subject).await
        }
        RunTransport::Http => run_command_h2(prepared, subject, &options.host, options.port).await,
    }
}

async fn run_command_stdio(spec: CommandSpec, subject: &str) -> Result<ExitStatus> {
    let program_display = spec.program.to_string_lossy().into_owned();
    let mut command = Command::new(&spec.program);
    apply_command_spec(&mut command, &spec);
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    command.status().await.with_context(|| {
        format!("failed to run {program_display} for {subject}")
    })
}

async fn run_command_h2(
    prepared: PreparedCommand,
    subject: &str,
    host: &str,
    port: u16,
) -> Result<ExitStatus> {
    let bound_listener = bind_listener(host, port).await?;
    eprintln!(
        "Running {} over http://{} (HTTP/2 stream transport)",
        subject, bound_listener.display_address
    );

    let (socket, peer_address) =
        bound_listener
            .listener
            .accept()
            .await
            .with_context(|| {
                format!(
                    "failed to accept HTTP connection on {}",
                    bound_listener.display_address
                )
            })?;
    eprintln!("Accepted connection from {}", peer_address);

    serve_h2_connection(prepared, subject, socket).await
}

async fn serve_h2_connection(
    prepared: PreparedCommand,
    subject: &str,
    socket: TcpStream,
) -> Result<ExitStatus> {
    let _temp_dir = prepared.temp_dir;
    let mut child = spawn_stream_child(&prepared.spec, subject)?;
    let child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("child process missing piped stdin"))?;
    let child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("child process missing piped stdout"))?;

    let session = Arc::new(Mutex::new(Some(StreamSession {
        child_stdin,
        child_stdout,
    })));
    let service = service_fn(move |request| {
        let session = session.clone();
        async move { handle_h2_request(request, session).await }
    });

    let connection = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
        .serve_connection(TokioIo::new(socket), service);
    tokio::pin!(connection);

    tokio::select! {
        status = child.wait() => {
            let status = status.context("failed while waiting on child process")?;
            connection.as_mut().graceful_shutdown();
            connection
                .await
                .map_err(|source| anyhow!("HTTP/2 connection failed: {source}"))?;
            Ok(status)
        }
        connection_result = &mut connection => {
            match connection_result {
                Ok(()) => terminate_child(&mut child).await,
                Err(source) => {
                    let _ = terminate_child(&mut child).await;
                    Err(anyhow!("HTTP/2 connection failed: {source}"))
                }
            }
        }
    }
}

async fn handle_h2_request(
    request: Request<Incoming>,
    session: Arc<Mutex<Option<StreamSession>>>,
) -> Result<Response<ResponseBody>, Infallible> {
    if request.version() != Version::HTTP_2 {
        return Ok(text_response(
            StatusCode::HTTP_VERSION_NOT_SUPPORTED,
            "HTTP/2 is required for this transport.\n",
        ));
    }

    if request.method() != Method::POST {
        return Ok(text_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "POST is required for this transport.\n",
        ));
    }

    if !content_type_is_stream(request.headers().get(CONTENT_TYPE)) {
        return Ok(text_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("Content-Type: {H2_STREAM_CONTENT_TYPE} is required.\n"),
        ));
    }

    let Some(session) = session.lock().await.take() else {
        return Ok(text_response(
            StatusCode::CONFLICT,
            "This transport accepts only one active HTTP/2 stream.\n",
        ));
    };

    let (response_tx, response_rx) =
        tokio::sync::mpsc::channel::<ResponseFrame>(STREAM_BUFFER_CAPACITY);
    tokio::spawn(pump_request_body_to_child(
        request.into_body(),
        session.child_stdin,
    ));
    tokio::spawn(pump_child_stdout_to_response(
        session.child_stdout,
        response_tx,
    ));

    Ok(stream_response(response_rx))
}

async fn pump_request_body_to_child(mut body: Incoming, mut child_stdin: ChildStdin) {
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else {
            break;
        };

        let Ok(data) = frame.into_data() else {
            continue;
        };

        if child_stdin.write_all(&data).await.is_err() {
            break;
        }
    }

    let _ = child_stdin.shutdown().await;
}

async fn pump_child_stdout_to_response(
    mut child_stdout: ChildStdout,
    response_tx: tokio::sync::mpsc::Sender<ResponseFrame>,
) {
    let mut buffer = [0_u8; COPY_BUFFER_SIZE];

    loop {
        let read = match child_stdout.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => break,
        };

        if response_tx
            .send(Ok(Frame::data(Bytes::copy_from_slice(&buffer[..read]))))
            .await
            .is_err()
        {
            break;
        }
    }
}

fn stream_response(
    response_rx: tokio::sync::mpsc::Receiver<ResponseFrame>,
) -> Response<ResponseBody> {
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, H2_STREAM_CONTENT_TYPE)
        .body(StreamBody::new(ReceiverStream::new(response_rx)).boxed())
        .expect("known-good response")
}

fn text_response(status: StatusCode, message: impl Into<Bytes>) -> Response<ResponseBody> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Full::new(message.into()).boxed())
        .expect("known-good response")
}

fn content_type_is_stream(content_type: Option<&hyper::header::HeaderValue>) -> bool {
    content_type
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case(H2_STREAM_CONTENT_TYPE))
}

fn spawn_stream_child(spec: &CommandSpec, subject: &str) -> Result<Child> {
    let program_display = spec.program.to_string_lossy().into_owned();
    let mut command = Command::new(&spec.program);
    apply_command_spec(&mut command, spec);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    command
        .spawn()
        .with_context(|| format!("failed to spawn {program_display} for {subject}"))
}

async fn terminate_child(child: &mut Child) -> Result<ExitStatus> {
    if let Some(status) = child
        .try_wait()
        .context("failed while checking child process status")?
    {
        return Ok(status);
    }

    child.kill().await.context("failed to terminate child process")?;
    child.wait().await.context("failed while waiting on child process")
}

async fn bind_listener(host: &str, port: u16) -> Result<BoundListener> {
    let listener = TcpListener::bind((host, port))
        .await
        .with_context(|| format!("failed to bind HTTP listener on {}", bind_target_display(host, port)))?;
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

fn package_command_spec(
    program: &str,
    package: &str,
    args: Option<&Vec<String>>,
    env: Option<&Environment>,
    user_args: &[String],
) -> CommandSpec {
    let mut command_args = vec![OsString::from(package)];
    if let Some(args) = args {
        command_args.extend(args.iter().cloned().map(OsString::from));
    }
    command_args.extend(user_args.iter().cloned().map(OsString::from));

    CommandSpec {
        program: OsString::from(program),
        args: command_args,
        env: clone_env_pairs(env),
        current_dir: None,
    }
}

fn binary_command_spec(
    executable_path: PathBuf,
    extracted_dir: PathBuf,
    target: &BinaryTarget,
    user_args: &[String],
) -> CommandSpec {
    let mut args: Vec<OsString> = target
        .args
        .as_ref()
        .into_iter()
        .flatten()
        .cloned()
        .map(OsString::from)
        .collect();
    args.extend(user_args.iter().cloned().map(OsString::from));

    CommandSpec {
        program: executable_path.into_os_string(),
        args,
        env: clone_env_pairs(target.env.as_ref()),
        current_dir: Some(extracted_dir),
    }
}

fn clone_env_pairs(env: Option<&Environment>) -> Vec<(OsString, OsString)> {
    env.into_iter()
        .flat_map(|pairs| pairs.iter())
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        .collect()
}

fn apply_command_spec(command: &mut Command, spec: &CommandSpec) {
    command.args(&spec.args);

    if let Some(current_dir) = &spec.current_dir {
        command.current_dir(current_dir);
    }

    for (key, value) in &spec.env {
        command.env(key, value);
    }
}

struct StreamSession {
    child_stdin: ChildStdin,
    child_stdout: ChildStdout,
}

struct BoundListener {
    listener: TcpListener,
    display_address: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandSpec {
    program: OsString,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    current_dir: Option<PathBuf>,
}

#[derive(Debug)]
struct PreparedCommand {
    spec: CommandSpec,
    temp_dir: Option<tempfile::TempDir>,
}

fn run_error_from_install(error: InstallError) -> anyhow::Error {
    match error {
        InstallError::InvalidArchiveUrl(url) => anyhow!("invalid archive URL: {url}"),
        InstallError::BinaryNotFound {
            archive,
            cmd,
            resolved_path,
        } => anyhow!(
            "downloaded {archive}, but could not find \"{cmd}\" at {}",
            resolved_path.display()
        ),
        InstallError::UnsupportedArchiveFormat(path) => {
            anyhow!("unsupported archive format for {path}")
        }
        InstallError::UnsafeArchiveEntry(entry) => anyhow!("archive contains unsafe path: {entry}"),
        InstallError::Request(error) => error.into(),
        InstallError::Io(error) => error.into(),
        InstallError::Zip(error) => error.into(),
        InstallError::Join(error) => anyhow!("blocking task failed: {error}"),
        InstallError::FetchRegistry(error) => error.into(),
        InstallError::AgentNotFound(error) => error.into(),
        InstallError::UnsupportedPlatform(error) => error.into(),
        InstallError::UnavailableDistribution { agent_id } => {
            anyhow!("agent \"{agent_id}\" does not have a runnable distribution")
        }
        InstallError::InvalidCommandPath(cmd) => anyhow!("invalid binary command path: {cmd}"),
        InstallError::MissingHomeDirectory => anyhow!(
            "could not determine the current user's home directory"
        ),
        InstallError::CommandIo { program, source } => {
            anyhow!("failed to run {program}: {source}")
        }
        InstallError::CommandFailed {
            program,
            subject,
            detail,
        } => anyhow!("failed to run {program} for {subject}: {detail}"),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::net::SocketAddr;
    use std::time::Duration;

    use super::*;

    #[test]
    fn accepts_stream_content_type() {
        let header = hyper::header::HeaderValue::from_static(H2_STREAM_CONTENT_TYPE);
        assert!(content_type_is_stream(Some(&header)));
    }

    #[test]
    fn rejects_non_stream_content_type() {
        let header = hyper::header::HeaderValue::from_static("application/json");
        assert!(!content_type_is_stream(Some(&header)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn http_transport_streams_full_duplex_over_h2() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            run_command_h2_with_listener(
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

        let (mut sender, connection) = h2_handshake(address).await;
        tokio::spawn(async move {
            let _ = connection.await;
        });

        let (request_tx, request_body) = request_body_channel();
        let request: Request<ResponseBody> = Request::builder()
            .method(Method::POST)
            .uri(format!("http://{address}/"))
            .version(Version::HTTP_2)
            .header(CONTENT_TYPE, H2_STREAM_CONTENT_TYPE)
            .body(request_body)
            .unwrap();

        let response = sender.send_request(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.version(), Version::HTTP_2);

        let mut response_body = response.into_body();
        let first_chunk = tokio::time::timeout(
            Duration::from_secs(2),
            read_next_data_frame(&mut response_body),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(first_chunk.as_ref(), b"boot\n");

        request_tx
            .send(Ok(Frame::data(Bytes::from_static(b"ping\n"))))
            .await
            .unwrap();
        drop(request_tx);

        let rest = response_body.collect().await.unwrap().to_bytes();
        assert_eq!(rest.as_ref(), b"ping\n");

        let status = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn http_transport_rejects_second_stream() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            run_command_h2_with_listener(
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

        let (mut sender, connection) = h2_handshake(address).await;
        tokio::spawn(async move {
            let _ = connection.await;
        });

        let (request_tx, request_body) = request_body_channel();
        let first: Request<ResponseBody> = Request::builder()
            .method(Method::POST)
            .uri(format!("http://{address}/"))
            .version(Version::HTTP_2)
            .header(CONTENT_TYPE, H2_STREAM_CONTENT_TYPE)
            .body(request_body)
            .unwrap();
        let first_response = sender.send_request(first).await.unwrap();
        assert_eq!(first_response.status(), StatusCode::OK);

        let second: Request<ResponseBody> = Request::builder()
            .method(Method::POST)
            .uri(format!("http://{address}/"))
            .version(Version::HTTP_2)
            .header(CONTENT_TYPE, H2_STREAM_CONTENT_TYPE)
            .body(Full::new(Bytes::new()).boxed())
            .unwrap();
        let second_response = sender.send_request(second).await.unwrap();
        assert_eq!(second_response.status(), StatusCode::CONFLICT);

        request_tx
            .send(Ok(Frame::data(Bytes::from_static(b"done"))))
            .await
            .unwrap();
        drop(request_tx);

        let _ = first_response.into_body().collect().await.unwrap();

        let status = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(status.success());
    }

    async fn h2_handshake(
        address: SocketAddr,
    ) -> (
        hyper::client::conn::http2::SendRequest<ResponseBody>,
        hyper::client::conn::http2::Connection<TokioIo<TcpStream>, ResponseBody, TokioExecutor>,
    ) {
        let stream = TcpStream::connect(address).await.unwrap();
        hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .handshake(TokioIo::new(stream))
            .await
            .unwrap()
    }

    fn request_body_channel() -> (tokio::sync::mpsc::Sender<ResponseFrame>, ResponseBody) {
        let (tx, rx) = tokio::sync::mpsc::channel::<ResponseFrame>(STREAM_BUFFER_CAPACITY);
        (tx, StreamBody::new(ReceiverStream::new(rx)).boxed())
    }

    async fn read_next_data_frame(body: &mut Incoming) -> Option<Bytes> {
        while let Some(frame) = body.frame().await {
            let frame = frame.unwrap();
            if let Ok(data) = frame.into_data() {
                return Some(data);
            }
        }

        None
    }

    #[cfg(unix)]
    async fn run_command_h2_with_listener(
        prepared: PreparedCommand,
        subject: &str,
        bound_listener: BoundListener,
    ) -> Result<ExitStatus> {
        eprintln!(
            "Running {} over http://{} (HTTP/2 stream transport)",
            subject, bound_listener.display_address
        );

        let (socket, _) =
            bound_listener
                .listener
                .accept()
                .await
                .with_context(|| {
                    format!(
                        "failed to accept HTTP connection on {}",
                        bound_listener.display_address
                    )
                })?;

        serve_h2_connection(prepared, subject, socket).await
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
