#![cfg(unix)]

mod support;

use std::ffi::OsString;

use acp_agent::runtime::transports::tcp::serve_tcp;
use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use support::transport::{prepared_command_with_program, reserve_local_port, timeout};

#[tokio::test]
async fn tcp_transport_streams_raw_stdio_over_socket() {
    let port = reserve_local_port();
    let server = tokio::spawn(async move {
        serve_tcp(
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

    let mut client = connect_tcp(port).await;
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
