#![cfg(unix)]

mod support;

use std::ffi::OsString;

use acp_agent::runtime::transports::ws::{
    StreamChunk, WS_CLOSE_STDIN_METHOD, WS_SUBSCRIBE_STDOUT_METHOD, WS_UNSUBSCRIBE_STDOUT_METHOD,
    WS_WRITE_STDIN_METHOD, serve_ws_connection,
};
use anyhow::{Context, Result};
use jsonrpsee::core::client::{ClientT, SubscriptionClientT};
use jsonrpsee::rpc_params;
use jsonrpsee::ws_client::WsClientBuilder;
use tokio::net::TcpListener;

use support::transport::{prepared_command_with_program, timeout};

#[tokio::test]
async fn ws_transport_streams_full_duplex_over_jsonrpc() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener
            .accept()
            .await
            .context("failed to accept WebSocket client")?;
        serve_ws_connection(
            prepared_command_with_program(
                OsString::from("sh"),
                vec![
                    OsString::from("-c"),
                    OsString::from("printf 'boot\\n'; cat"),
                ],
            )
            .spec,
            "demo-agent",
            socket,
        )
        .await
    });

    let client = WsClientBuilder::default()
        .build(format!("ws://{address}"))
        .await
        .unwrap();
    let mut stdout = client
        .subscribe::<StreamChunk, _>(
            WS_SUBSCRIBE_STDOUT_METHOD,
            rpc_params![],
            WS_UNSUBSCRIBE_STDOUT_METHOD,
        )
        .await
        .unwrap();

    let first_chunk = timeout(stdout.next()).await.unwrap().unwrap();
    assert_eq!(first_chunk.data, b"boot\n");

    let written: usize = client
        .request(
            WS_WRITE_STDIN_METHOD,
            rpc_params![StreamChunk {
                data: b"ping\n".to_vec(),
            }],
        )
        .await
        .unwrap();
    assert_eq!(written, 5);

    let closed: bool = client
        .request(WS_CLOSE_STDIN_METHOD, rpc_params![])
        .await
        .unwrap();
    assert!(closed);

    let echoed = timeout(stdout.next()).await.unwrap().unwrap();
    assert_eq!(echoed.data, b"ping\n");

    let status: Result<_> = timeout(server).await.unwrap();
    assert!(status.unwrap().success());
}

#[tokio::test]
async fn ws_transport_rejects_second_stdout_subscription() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener
            .accept()
            .await
            .context("failed to accept WebSocket client")?;
        serve_ws_connection(
            prepared_command_with_program(
                OsString::from("sh"),
                vec![OsString::from("-c"), OsString::from("cat")],
            )
            .spec,
            "demo-agent",
            socket,
        )
        .await
    });

    let client = WsClientBuilder::default()
        .build(format!("ws://{address}"))
        .await
        .unwrap();
    let first_subscription = client
        .subscribe::<StreamChunk, _>(
            WS_SUBSCRIBE_STDOUT_METHOD,
            rpc_params![],
            WS_UNSUBSCRIBE_STDOUT_METHOD,
        )
        .await;
    assert!(first_subscription.is_ok());

    let second_subscription = client
        .subscribe::<StreamChunk, _>(
            WS_SUBSCRIBE_STDOUT_METHOD,
            rpc_params![],
            WS_UNSUBSCRIBE_STDOUT_METHOD,
        )
        .await;
    assert!(second_subscription.is_err());

    let closed: bool = client
        .request(WS_CLOSE_STDIN_METHOD, rpc_params![])
        .await
        .unwrap();
    assert!(closed);
    drop(first_subscription);

    let status: Result<_> = timeout(server).await.unwrap();
    assert!(status.unwrap().success());
}
