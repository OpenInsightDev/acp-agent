#![cfg(unix)]

mod support;

use std::fs;

use acp_agent::runtime::transports::uds::serve_uds;
use anyhow::Result;
use support::transport::{prepared_command, timeout};
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[tokio::test]
async fn uds_transport_streams_raw_stdio_over_socket() {
    let temp_dir = tempdir().unwrap();
    let socket_path = temp_dir.path().join("agent.sock");
    let server_socket_path = socket_path.clone();

    let server = tokio::spawn(async move {
        serve_uds(
            prepared_command("sh", &["-c", "printf 'boot\\n'; cat"]),
            "demo-agent",
            &server_socket_path,
        )
        .await
    });

    wait_for_socket_path(&socket_path).await;

    let mut client = UnixStream::connect(&socket_path).await.unwrap();
    let mut first_chunk = [0_u8; 5];
    timeout(client.read_exact(&mut first_chunk), "UDS greeting")
        .await
        .unwrap();
    assert_eq!(&first_chunk, b"boot\n");

    client.write_all(b"ping\n").await.unwrap();
    client.shutdown().await.unwrap();

    let mut echoed = Vec::new();
    timeout(client.read_to_end(&mut echoed), "UDS echo")
        .await
        .unwrap();
    assert_eq!(echoed, b"ping\n");

    let status: Result<_> = timeout(server, "UDS server task").await.unwrap();
    assert!(status.unwrap().success());
}

#[tokio::test]
async fn uds_bind_fails_when_socket_path_exists() {
    let temp_dir = tempdir().unwrap();
    let socket_path = temp_dir.path().join("agent.sock");
    fs::write(&socket_path, b"occupied").unwrap();

    let error = serve_uds(
        prepared_command("sh", &["-c", "cat"]),
        "demo-agent",
        &socket_path,
    )
    .await
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("refusing to bind Unix socket at")
    );
}

#[tokio::test]
async fn uds_bind_does_not_remove_preexisting_path() {
    let temp_dir = tempdir().unwrap();
    let socket_path = temp_dir.path().join("agent.sock");
    fs::write(&socket_path, b"occupied").unwrap();

    let _ = serve_uds(
        prepared_command("sh", &["-c", "cat"]),
        "demo-agent",
        &socket_path,
    )
    .await
    .unwrap_err();

    assert!(socket_path.exists());
    assert_eq!(fs::read(&socket_path).unwrap(), b"occupied");
}

#[tokio::test]
async fn uds_socket_file_is_removed_after_clean_shutdown() {
    let temp_dir = tempdir().unwrap();
    let socket_path = temp_dir.path().join("agent.sock");
    let server_socket_path = socket_path.clone();

    let server = tokio::spawn(async move {
        serve_uds(
            prepared_command("sh", &["-c", "printf 'boot\\n'; cat"]),
            "demo-agent",
            &server_socket_path,
        )
        .await
    });

    wait_for_socket_path(&socket_path).await;

    let mut client = UnixStream::connect(&socket_path).await.unwrap();
    let mut first_chunk = [0_u8; 5];
    timeout(client.read_exact(&mut first_chunk), "UDS greeting")
        .await
        .unwrap();
    assert_eq!(&first_chunk, b"boot\n");

    client.write_all(b"ping\n").await.unwrap();
    client.shutdown().await.unwrap();

    let mut echoed = Vec::new();
    timeout(client.read_to_end(&mut echoed), "UDS echo")
        .await
        .unwrap();
    assert_eq!(echoed, b"ping\n");

    let status: Result<_> = timeout(server, "UDS server task").await.unwrap();
    assert!(status.unwrap().success());
    assert!(!socket_path.exists());
}

async fn wait_for_socket_path(socket_path: &std::path::Path) {
    timeout(
        async {
            loop {
                if socket_path.exists() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        },
        "UDS socket bind",
    )
    .await;
}
