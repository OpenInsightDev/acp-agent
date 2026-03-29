#![cfg(unix)]

mod support;

use std::ffi::OsString;
use std::net::SocketAddr;

use acp_agent::runtime::transports::h2::{H2_STREAM_CONTENT_TYPE, serve_h2_connection};
use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::header::CONTENT_TYPE;
use hyper::{Method, Request, StatusCode, Version};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::{TcpListener, TcpStream};
use tokio_stream::wrappers::ReceiverStream;

use support::transport::{prepared_command_with_program, timeout};

type ResponseFrame = Result<Frame<Bytes>, std::convert::Infallible>;
type ResponseBody = BoxBody<Bytes, std::convert::Infallible>;

#[tokio::test]
async fn http_transport_streams_full_duplex_over_h2() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.context("failed to accept HTTP client")?;
        serve_h2_connection(
            prepared_command_with_program(
                OsString::from("sh"),
                vec![
                    OsString::from("-c"),
                    OsString::from("printf 'boot\\n'; cat"),
                ],
            ),
            "demo-agent",
            socket,
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
    let first_chunk = timeout(read_next_data_frame(&mut response_body))
        .await
        .unwrap();
    assert_eq!(first_chunk.as_ref(), b"boot\n");

    request_tx
        .send(Ok(Frame::data(Bytes::from_static(b"ping\n"))))
        .await
        .unwrap();
    drop(request_tx);

    let rest = response_body.collect().await.unwrap().to_bytes();
    assert_eq!(rest.as_ref(), b"ping\n");

    let status: Result<_> = timeout(server).await.unwrap();
    assert!(status.unwrap().success());
}

#[tokio::test]
async fn http_transport_rejects_second_stream() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.context("failed to accept HTTP client")?;
        serve_h2_connection(
            prepared_command_with_program(
                OsString::from("sh"),
                vec![OsString::from("-c"), OsString::from("cat")],
            ),
            "demo-agent",
            socket,
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

    let status: Result<_> = timeout(server).await.unwrap();
    assert!(status.unwrap().success());
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
    let (tx, rx) = tokio::sync::mpsc::channel::<ResponseFrame>(16);
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
