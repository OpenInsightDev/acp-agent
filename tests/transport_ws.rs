#![cfg(unix)]

mod support;

use std::ffi::OsString;

use acp_agent::runtime::transports::ws::serve_ws;
use anyhow::Result;
use jsonrpsee::core::client::{ClientT, SubscriptionClientT};
use jsonrpsee::rpc_params;
use jsonrpsee::ws_client::{WsClient, WsClientBuilder};
use serde::{Deserialize, Serialize};

use support::transport::{prepared_command_with_program, reserve_local_port, timeout};

const WS_WRITE_STDIN_METHOD: &str = "write_stdin";
const WS_CLOSE_STDIN_METHOD: &str = "close_stdin";
const WS_SUBSCRIBE_STDOUT_METHOD: &str = "subscribe_stdout";
const WS_UNSUBSCRIBE_STDOUT_METHOD: &str = "unsubscribe_stdout";

#[tokio::test]
async fn ws_transport_streams_full_duplex_over_jsonrpc() {
    let port = reserve_local_port();
    let server = tokio::spawn(async move {
        serve_ws(
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

    let client = connect_ws(port).await;
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
    let port = reserve_local_port();
    let server = tokio::spawn(async move {
        serve_ws(
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

    let client = connect_ws(port).await;
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StreamChunk {
    data: Vec<u8>,
}

async fn connect_ws(port: u16) -> WsClient {
    let url = format!("ws://127.0.0.1:{port}");
    timeout(async {
        loop {
            match WsClientBuilder::default().build(&url).await {
                Ok(client) => break client,
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
    })
    .await
}
