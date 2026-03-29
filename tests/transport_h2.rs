#![cfg(unix)]

mod support;

use std::ffi::OsString;

use acp_agent::runtime::transports::h2::serve_h2;
use anyhow::Result;
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::header::CONTENT_TYPE;
use hyper::{Method, Request, StatusCode, Version};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;

use support::transport::{
    STREAM_BUFFER_CAPACITY, prepared_command_with_program, reserve_local_port, timeout,
};

const H2_STREAM_CONTENT_TYPE: &str = "application/octet-stream";

type ResponseFrame = Result<Frame<Bytes>, std::convert::Infallible>;
type ResponseBody = BoxBody<Bytes, std::convert::Infallible>;

#[tokio::test]
async fn http_transport_streams_full_duplex_over_h2() {
    let port = reserve_local_port();
    let address = format!("127.0.0.1:{port}");
    let server = tokio::spawn(async move {
        serve_h2(
            prepared_command_with_program(
                OsString::from("sh"),
                vec![
                    OsString::from("-c"),
                    OsString::from("printf 'boot\\n'; cat"),
                ],
            ),
            "demo-agent",
            "127.0.0.1",
            port,
        )
        .await
    });

    let (mut sender, connection_task) = h2_handshake(port).await;

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

    assert_h2_connection_ok(connection_task).await;
    let status: Result<_> = timeout(server).await.unwrap();
    assert!(status.unwrap().success());
}

#[tokio::test]
async fn http_transport_rejects_second_stream() {
    let port = reserve_local_port();
    let address = format!("127.0.0.1:{port}");
    let server = tokio::spawn(async move {
        serve_h2(
            prepared_command_with_program(
                OsString::from("sh"),
                vec![OsString::from("-c"), OsString::from("cat")],
            ),
            "demo-agent",
            "127.0.0.1",
            port,
        )
        .await
    });

    let (mut sender, connection_task) = h2_handshake(port).await;

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

    assert_h2_connection_ok(connection_task).await;
    let status: Result<_> = timeout(server).await.unwrap();
    assert!(status.unwrap().success());
}

async fn h2_handshake(port: u16) -> (
    hyper::client::conn::http2::SendRequest<ResponseBody>,
    JoinHandle<Result<(), hyper::Error>>,
) {
    let stream = connect_tcp(port).await;
    let (sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake(TokioIo::new(stream))
        .await
        .unwrap();
    let connection_task = tokio::spawn(connection);

    (sender, connection_task)
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

async fn connect_tcp(port: u16) -> TcpStream {
    let address = format!("127.0.0.1:{port}");
    timeout(async {
        loop {
            match TcpStream::connect(&address).await {
                Ok(stream) => break stream,
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
    })
    .await
}

async fn assert_h2_connection_ok(connection_task: JoinHandle<Result<(), hyper::Error>>) {
    timeout(async {
        let result = connection_task.await.expect("failed to join h2 connection task");
        result.expect("h2 connection task failed");
    })
    .await;
}
