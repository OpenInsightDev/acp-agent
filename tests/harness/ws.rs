use std::time::Duration;

use async_tungstenite::WebSocketStream;
use async_tungstenite::tokio::{ConnectStream, connect_async};
use async_tungstenite::tungstenite::client::IntoClientRequest;
use async_tungstenite::tungstenite::handshake::client::Response;
use async_tungstenite::tungstenite::{Error, Message};
use futures::StreamExt;
use reqwest::header::ORIGIN;

/// How long a single WebSocket read waits before a case fails.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// A connected WebSocket client speaking text frames, for the ACP endpoint's
/// WebSocket transport.
#[derive(Debug)]
pub struct WsClient {
    socket: WebSocketStream<ConnectStream>,
    response: Response,
}

impl WsClient {
    /// Connects to `url` without a browser `Origin`, as a non-browser client does.
    pub async fn connect(url: &str) -> Self {
        Self::try_connect(url).await.unwrap_or_else(|status| {
            panic!("WebSocket handshake with {url} was rejected with {status}")
        })
    }

    /// Connects without a browser `Origin`, returning the HTTP status when the
    /// server rejects the handshake.
    pub async fn try_connect(url: &str) -> Result<Self, u16> {
        Self::handshake(url, None).await
    }

    /// Connects with a browser `Origin` header, as a page in a browser would,
    /// returning the HTTP status when the origin policy rejects the handshake.
    pub async fn try_connect_with_origin(url: &str, origin: &str) -> Result<Self, u16> {
        Self::handshake(url, Some(origin)).await
    }

    async fn handshake(url: &str, origin: Option<&str>) -> Result<Self, u16> {
        let mut request = url
            .into_client_request()
            .unwrap_or_else(|error| panic!("invalid WebSocket URL {url}: {error}"));
        if let Some(origin) = origin {
            request.headers_mut().insert(
                ORIGIN,
                origin
                    .parse()
                    .unwrap_or_else(|error| panic!("invalid origin {origin}: {error}")),
            );
        }
        match connect_async(request).await {
            Ok((socket, response)) => Ok(Self { socket, response }),
            Err(Error::Http(response)) => Err(response.status().as_u16()),
            Err(error) => panic!("unexpected WebSocket handshake failure: {error}"),
        }
    }

    /// Header the server returned in the handshake response, such as the ACP
    /// connection id.
    pub fn handshake_header(&self, name: &str) -> Option<&str> {
        self.response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
    }

    /// Sends one text frame, then returns the next text frame the server sends.
    pub async fn send_and_receive(&mut self, payload: &str) -> String {
        self.send_text(payload).await;
        self.next_text().await
    }

    pub async fn send_text(&mut self, payload: &str) {
        self.socket
            .send(Message::Text(payload.into()))
            .await
            .expect("failed to send a WebSocket frame");
    }

    /// Next text frame from the server, panicking on a non-text frame or timeout.
    pub async fn next_text(&mut self) -> String {
        let frame = tokio::time::timeout(FRAME_TIMEOUT, self.socket.next())
            .await
            .expect("the WebSocket server did not answer in time")
            .expect("the WebSocket server closed the connection")
            .expect("the WebSocket frame could not be read");
        match frame {
            Message::Text(text) => text.to_string(),
            other => panic!("expected a text frame, got {other:?}"),
        }
    }

    pub async fn close(mut self) {
        let _ = self.socket.close(None).await;
    }
}
