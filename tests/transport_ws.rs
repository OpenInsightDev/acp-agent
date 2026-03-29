//! Integration tests for the WebSocket transport.
//!
//! Wire protocol: one ACP JSON-RPC message = one WebSocket `Text` frame.
//! Child stdio uses NDJSON framing (newline-delimited).
#![cfg(unix)]

mod support;

use std::ffi::OsString;

use acp_agent::runtime::transports::ws::serve_ws_connection;
use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use support::transport::{prepared_command_with_program, timeout};

// ── framing happy path ─────────────────────────────────────────────────────────

#[tokio::test]
async fn ws_transport_single_text_frame_is_forwarded_as_ndjson_line() {
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

    let (mut ws, _) = connect_async(format!("ws://{address}")).await.unwrap();

    ws.send(Message::Text(r#"{"jsonrpc":"2.0","method":"ping"}"#.into()))
        .await
        .unwrap();

    let reply = timeout(ws.next()).await.unwrap().unwrap();
    assert_eq!(
        reply,
        Message::Text(r#"{"jsonrpc":"2.0","method":"ping"}"#.into())
    );

    ws.close(None).await.unwrap();
    let status = timeout(server).await.unwrap().unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn ws_transport_child_ndjson_output_becomes_separate_frames() {
    // Child writes two NDJSON messages in one printf, then exits.
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
                    OsString::from(r#"printf '{"id":1}\n{"id":2}\n'"#),
                ],
            )
            .spec,
            "demo-agent",
            socket,
        )
        .await
    });

    let (mut ws, _) = connect_async(format!("ws://{address}")).await.unwrap();

    let f1 = timeout(ws.next()).await.unwrap().unwrap();
    let f2 = timeout(ws.next()).await.unwrap().unwrap();
    assert_eq!(f1, Message::Text(r#"{"id":1}"#.into()));
    assert_eq!(f2, Message::Text(r#"{"id":2}"#.into()));

    let _ = timeout(server).await.unwrap().unwrap();
}

#[tokio::test]
async fn ws_transport_empty_ndjson_lines_are_skipped() {
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
                    OsString::from(r#"printf '{"id":1}\n\n{"id":2}\n'"#),
                ],
            )
            .spec,
            "demo-agent",
            socket,
        )
        .await
    });

    let (mut ws, _) = connect_async(format!("ws://{address}")).await.unwrap();

    let f1 = timeout(ws.next()).await.unwrap().unwrap();
    let f2 = timeout(ws.next()).await.unwrap().unwrap();
    assert_eq!(f1, Message::Text(r#"{"id":1}"#.into()));
    assert_eq!(f2, Message::Text(r#"{"id":2}"#.into()));

    let _ = timeout(server).await.unwrap().unwrap();
}

// ── inbound validation ────────────────────────────────────────────────────────

#[tokio::test]
async fn ws_transport_rejects_binary_frame() {
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

    let (mut ws, _) = connect_async(format!("ws://{address}")).await.unwrap();
    ws.send(Message::Binary(b"data".to_vec().into()))
        .await
        .unwrap();

    let result = timeout(server).await.unwrap();
    assert!(result.is_err(), "expected error for Binary frame");
}

#[tokio::test]
async fn ws_transport_rejects_frame_containing_newline() {
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

    let (mut ws, _) = connect_async(format!("ws://{address}")).await.unwrap();
    ws.send(Message::Text("{\"a\":1}\n{\"b\":2}".into()))
        .await
        .unwrap();

    let result = timeout(server).await.unwrap();
    assert!(result.is_err(), "expected error for frame with newline");
}

#[tokio::test]
async fn ws_transport_rejects_non_json_frame() {
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

    let (mut ws, _) = connect_async(format!("ws://{address}")).await.unwrap();
    ws.send(Message::Text("not json".into())).await.unwrap();

    let result = timeout(server).await.unwrap();
    assert!(result.is_err(), "expected error for non-JSON frame");
}

#[tokio::test]
async fn ws_transport_rejects_json_array_frame() {
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

    let (mut ws, _) = connect_async(format!("ws://{address}")).await.unwrap();
    ws.send(Message::Text("[1,2,3]".into())).await.unwrap();

    let result = timeout(server).await.unwrap();
    assert!(result.is_err(), "expected error for JSON array frame");
}

// ── outbound failure paths ────────────────────────────────────────────────────

#[tokio::test]
async fn ws_transport_fails_when_child_stdout_has_no_trailing_newline() {
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
                vec![OsString::from("-c"), OsString::from(r#"printf '{"id":1}'"#)],
            )
            .spec,
            "demo-agent",
            socket,
        )
        .await
    });

    let (_ws, _) = connect_async(format!("ws://{address}")).await.unwrap();

    let result = timeout(server).await.unwrap();
    assert!(
        result.is_err(),
        "expected error for unterminated stdout line"
    );
}

#[tokio::test]
async fn ws_transport_closes_cleanly_when_child_exits_normally() {
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
                    OsString::from(r#"printf '{"ok":true}\n'"#),
                ],
            )
            .spec,
            "demo-agent",
            socket,
        )
        .await
    });

    let (mut ws, _) = connect_async(format!("ws://{address}")).await.unwrap();

    let frame = timeout(ws.next()).await.unwrap().unwrap();
    assert_eq!(frame, Message::Text(r#"{"ok":true}"#.into()));

    // WS should close gracefully after child exits.
    let next = timeout(ws.next()).await;
    if let Some(Ok(msg)) = next {
        assert!(matches!(msg, Message::Close(_)));
    }

    let status = timeout(server).await.unwrap().unwrap();
    assert!(status.success());
}
