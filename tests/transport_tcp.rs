#![cfg(unix)]

mod support;

use std::ffi::OsString;

use acp_agent::runtime::transports::tcp::serve_tcp_connection;
use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use support::transport::{prepared_command_with_program, timeout};

#[tokio::test]
async fn tcp_transport_streams_raw_stdio_over_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener
            .accept()
            .await
            .context("failed to accept TCP client")?;
        serve_tcp_connection(
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

    let mut client = TcpStream::connect(address).await.unwrap();
    let mut first_chunk = [0_u8; 5];
    timeout(client.read_exact(&mut first_chunk)).await.unwrap();
    assert_eq!(&first_chunk, b"boot\n");

    client.write_all(b"ping\n").await.unwrap();
    client.shutdown().await.unwrap();

    let mut echoed = Vec::new();
    timeout(client.read_to_end(&mut echoed)).await.unwrap();
    assert_eq!(echoed, b"ping\n");

    let status: Result<_> = timeout(server).await.unwrap();
    assert!(status.unwrap().success());
}
