//! HTTP/2 transport that upgrades the agent's stdio into a bidirectional data stream.
//! The connection accepts a single HTTP/2 stream with a fixed `Content-Type`.
use std::convert::Infallible;
use std::process::ExitStatus;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::header::CONTENT_TYPE;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Version};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::Mutex;
use tokio_stream::wrappers::ReceiverStream;

use crate::runtime::prepare::PreparedCommand;
use crate::runtime::process::{spawn_stream_child, terminate_child};

#[doc(hidden)]
pub const H2_STREAM_CONTENT_TYPE: &str = "application/octet-stream";
const STREAM_BUFFER_CAPACITY: usize = 16;
const COPY_BUFFER_SIZE: usize = 8192;

type ResponseBody = BoxBody<Bytes, Infallible>;
type ResponseFrame = Result<Frame<Bytes>, Infallible>;

/// Handles a single HTTP/2 POST request whose body is streamed to stdin.
///
/// Enforces `Content-Type: application/octet-stream` and requires HTTP/2. One
/// response stream mirrors stdout frames, and the transport shuts down when the
/// client or agent closes the stream.
pub async fn serve_h2(
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

    let (socket, peer_address) = bound_listener.listener.accept().await.with_context(|| {
        format!(
            "failed to accept HTTP connection on {}",
            bound_listener.display_address
        )
    })?;
    eprintln!("Accepted connection from {}", peer_address);

    serve_h2_connection(prepared, subject, socket).await
}

#[doc(hidden)]
pub async fn serve_h2_connection(
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

async fn bind_listener(host: &str, port: u16) -> Result<BoundListener> {
    let listener = TcpListener::bind((host, port)).await.with_context(|| {
        format!(
            "failed to bind HTTP listener on {}",
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

struct StreamSession {
    child_stdin: ChildStdin,
    child_stdout: ChildStdout,
}

struct BoundListener {
    listener: TcpListener,
    display_address: String,
}

#[cfg(test)]
mod tests {
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
}
