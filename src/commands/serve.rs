use agent_client_protocol::{AcpAgent, AcpAgentConfig};
use agent_client_protocol_http::{AcpHttpServer, CorsOptions, ServerOptions};
use anyhow::{Context, Result, bail};
use tokio::net::TcpListener;

/// HTTP listener and ACP endpoint configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeOptions {
    /// Hostname or IP address to bind.
    pub host: String,
    /// TCP port to bind. Port `0` lets the operating system choose a port.
    pub port: u16,
    /// Path serving ACP over HTTP/SSE and WebSocket.
    pub path: String,
    /// Cross-origin browser access policy.
    pub cors: CorsOptions,
    /// Whether to expose `GET /health`.
    pub health_endpoint: bool,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 0,
            path: "/acp".to_string(),
            cors: CorsOptions::disabled(),
            health_endpoint: true,
        }
    }
}

pub(crate) fn cors_options(origins: Vec<String>, allow_any: bool) -> Result<CorsOptions> {
    if allow_any {
        Ok(CorsOptions::allow_any_origin())
    } else if origins.is_empty() {
        Ok(CorsOptions::disabled())
    } else {
        CorsOptions::allow_origins(origins)
            .context("CORS origin contains an invalid HTTP header value")
    }
}

/// Exposes a registry agent over ACP HTTP/SSE and WebSocket transports.
pub async fn serve_agent(agent_id: &str, options: ServeOptions, args: &[String]) -> Result<()> {
    let config = crate::runner::resolve_agent_config(agent_id, args).await?;
    serve_config(config, options).await
}

async fn serve_config(config: AcpAgentConfig, options: ServeOptions) -> Result<()> {
    let server_options = http_server_options(&options)?;
    let listener = TcpListener::bind((options.host.as_str(), options.port))
        .await
        .with_context(|| {
            format!(
                "failed to bind ACP HTTP listener on {}:{}",
                options.host, options.port
            )
        })?;
    let address = listener
        .local_addr()
        .context("failed to read ACP HTTP listener address")?;
    eprintln!(
        "Serving ACP agent at http://{address}{} (WebSocket available on the same endpoint)",
        options.path
    );
    serve_listener(listener, config, server_options).await
}

async fn serve_listener(
    listener: TcpListener,
    config: AcpAgentConfig,
    server_options: ServerOptions,
) -> Result<()> {
    let router = AcpHttpServer::new(move || AcpAgent::new(config.clone()))
        .with_options(server_options)
        .into_router();

    axum::serve(listener, router)
        .await
        .context("ACP HTTP server failed")
}

fn http_server_options(options: &ServeOptions) -> Result<ServerOptions> {
    if !options.path.starts_with('/') {
        bail!("ACP endpoint path must start with '/'");
    }
    if options.path.len() == 1 {
        bail!("ACP endpoint path cannot be '/'");
    }
    if options.health_endpoint && options.path == "/health" {
        bail!("ACP endpoint path conflicts with the health endpoint");
    }

    Ok(ServerOptions {
        path: options.path.clone(),
        cors: options.cors.clone(),
        health_endpoint: options.health_endpoint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_endpoint_and_cors_configuration() {
        for (path, expected) in [
            ("acp", "must start with '/'"),
            ("/", "cannot be '/'"),
            ("/health", "conflicts with the health endpoint"),
        ] {
            let error = http_server_options(&ServeOptions {
                path: path.to_string(),
                ..ServeOptions::default()
            })
            .unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }

        let error = cors_options(vec!["bad\norigin".to_string()], false).unwrap_err();
        assert!(error.to_string().contains("invalid HTTP header value"));
    }

    #[tokio::test]
    async fn reports_listener_bind_failure() {
        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = occupied.local_addr().unwrap();
        let error = serve_config(
            AcpAgentConfig::new("unused-agent"),
            ServeOptions {
                host: address.ip().to_string(),
                port: address.port(),
                ..ServeOptions::default()
            },
        )
        .await
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("failed to bind ACP HTTP listener")
        );
    }

    #[cfg(unix)]
    mod network {
        use std::net::SocketAddr;
        use std::time::Duration;

        use async_tungstenite::tokio::connect_async;
        use async_tungstenite::tungstenite::{Message, client::IntoClientRequest};
        use futures::StreamExt;
        use reqwest::header::{
            ACCEPT, ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_REQUEST_METHOD, CONTENT_TYPE,
            ORIGIN,
        };
        use serde_json::{Value, json};
        use tokio::time::timeout;

        use super::*;

        const CONNECTION_ID: &str = "acp-connection-id";
        const INITIALIZE_REQUEST: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}"#;
        const ECHO_REQUEST: &str = r#"{"jsonrpc":"2.0","id":2,"method":"test/echo","params":{}}"#;

        fn fixture_agent() -> AcpAgentConfig {
            AcpAgentConfig::new("/bin/sh").args([
                "-c",
                r#"while IFS= read -r line; do
case "$line" in
*'"id":2'*)
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"echo":"ok"}}'
;;
*)
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}'
;;
esac
done"#,
            ])
        }

        struct TestServer {
            address: SocketAddr,
            task: tokio::task::JoinHandle<()>,
        }

        impl TestServer {
            async fn start(options: ServeOptions) -> Self {
                Self::start_with_agent(options, fixture_agent()).await
            }

            async fn start_with_agent(options: ServeOptions, config: AcpAgentConfig) -> Self {
                let server_options = http_server_options(&options).unwrap();
                let listener = TcpListener::bind((options.host.as_str(), options.port))
                    .await
                    .unwrap();
                let address = listener.local_addr().unwrap();
                let task = tokio::spawn(async move {
                    serve_listener(listener, config, server_options)
                        .await
                        .unwrap();
                });
                Self { address, task }
            }

            fn http_url(&self, path: &str) -> String {
                format!("http://{}{path}", self.address)
            }

            fn ws_url(&self, path: &str) -> String {
                format!("ws://{}{path}", self.address)
            }
        }

        impl Drop for TestServer {
            fn drop(&mut self) {
                self.task.abort();
            }
        }

        async fn initialize_http(client: &reqwest::Client, endpoint: &str) -> reqwest::Response {
            timeout(
                Duration::from_secs(5),
                client
                    .post(endpoint)
                    .header(CONTENT_TYPE, "application/json")
                    .body(INITIALIZE_REQUEST)
                    .send(),
            )
            .await
            .expect("HTTP initialize timed out")
            .unwrap()
        }

        #[tokio::test]
        async fn serves_health_http_initialize_sse_and_delete_lifecycle() {
            let server = TestServer::start(ServeOptions::default()).await;
            let client = reqwest::Client::new();
            let endpoint = server.http_url("/acp");

            let health = client.get(server.http_url("/health")).send().await.unwrap();
            assert_eq!(health.status(), reqwest::StatusCode::OK);
            assert_eq!(health.text().await.unwrap(), "ok");

            let unsupported = client
                .post(&endpoint)
                .body(INITIALIZE_REQUEST)
                .send()
                .await
                .unwrap();
            assert_eq!(
                unsupported.status(),
                reqwest::StatusCode::UNSUPPORTED_MEDIA_TYPE
            );

            let initialized = initialize_http(&client, &endpoint).await;
            assert_eq!(initialized.status(), reqwest::StatusCode::OK);
            let connection_id = initialized
                .headers()
                .get(CONNECTION_ID)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            let response: Value = serde_json::from_str(&initialized.text().await.unwrap()).unwrap();
            assert_eq!(response["id"], 1);
            assert_eq!(response["result"]["protocolVersion"], 1);

            let second = initialize_http(&client, &endpoint).await;
            let second_connection_id = second
                .headers()
                .get(CONNECTION_ID)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            assert_ne!(connection_id, second_connection_id);
            drop(second);

            let sse = timeout(
                Duration::from_secs(5),
                client
                    .get(&endpoint)
                    .header(ACCEPT, "text/event-stream")
                    .header(CONNECTION_ID, &connection_id)
                    .send(),
            )
            .await
            .expect("SSE establishment timed out")
            .unwrap();
            assert_eq!(sse.status(), reqwest::StatusCode::OK);
            assert_eq!(
                sse.headers().get(CONTENT_TYPE).unwrap(),
                "text/event-stream"
            );
            let mut events = sse.bytes_stream();
            let accepted = client
                .post(&endpoint)
                .header(CONTENT_TYPE, "application/json")
                .header(CONNECTION_ID, &connection_id)
                .body(ECHO_REQUEST)
                .send()
                .await
                .unwrap();
            assert_eq!(accepted.status(), reqwest::StatusCode::ACCEPTED);
            let event = timeout(Duration::from_secs(5), events.next())
                .await
                .expect("SSE response timed out")
                .unwrap()
                .unwrap();
            let event = std::str::from_utf8(&event).unwrap();
            assert!(event.starts_with("data: "));
            assert!(event.contains(r#""id":2"#));
            assert!(event.contains(r#""echo":"ok""#));
            drop(events);

            let deleted = client
                .delete(&endpoint)
                .header(CONNECTION_ID, &connection_id)
                .send()
                .await
                .unwrap();
            assert_eq!(deleted.status(), reqwest::StatusCode::ACCEPTED);

            let missing = client
                .delete(&endpoint)
                .header(CONNECTION_ID, &connection_id)
                .send()
                .await
                .unwrap();
            assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);

            let second_deleted = client
                .delete(&endpoint)
                .header(CONNECTION_ID, second_connection_id)
                .send()
                .await
                .unwrap();
            assert_eq!(second_deleted.status(), reqwest::StatusCode::ACCEPTED);

            let missing_header = client.delete(&endpoint).send().await.unwrap();
            assert_eq!(missing_header.status(), reqwest::StatusCode::BAD_REQUEST);
        }

        #[tokio::test]
        async fn serves_websocket_on_the_acp_endpoint() {
            let server = TestServer::start(ServeOptions::default()).await;
            let (mut socket, response) = connect_async(server.ws_url("/acp")).await.unwrap();
            assert!(response.headers().contains_key(CONNECTION_ID));

            socket
                .send(Message::Text(INITIALIZE_REQUEST.into()))
                .await
                .unwrap();
            let frame = timeout(Duration::from_secs(5), socket.next())
                .await
                .expect("WebSocket initialize timed out")
                .unwrap()
                .unwrap();
            let Message::Text(text) = frame else {
                panic!("expected text response, got {frame:?}");
            };
            let response: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(response["result"]["protocolVersion"], json!(1));

            socket
                .send(Message::Text(ECHO_REQUEST.into()))
                .await
                .unwrap();
            let frame = timeout(Duration::from_secs(5), socket.next())
                .await
                .expect("WebSocket echo timed out")
                .unwrap()
                .unwrap();
            let Message::Text(text) = frame else {
                panic!("expected text response, got {frame:?}");
            };
            let response: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(response["id"], json!(2));
            assert_eq!(response["result"]["echo"], "ok");

            socket.close(None).await.unwrap();
        }

        #[tokio::test]
        async fn enforces_websocket_origin_policy() {
            let disabled_server = TestServer::start(ServeOptions::default()).await;
            let mut request = disabled_server
                .ws_url("/acp")
                .into_client_request()
                .unwrap();
            request
                .headers_mut()
                .insert(ORIGIN, "https://example.com".parse().unwrap());
            let error = connect_async(request).await.unwrap_err();
            let async_tungstenite::tungstenite::Error::Http(response) = error else {
                panic!("expected HTTP handshake rejection, got {error:?}");
            };
            assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);

            let allowed_options = ServeOptions {
                cors: cors_options(Vec::new(), true).unwrap(),
                ..ServeOptions::default()
            };
            let allowed_server = TestServer::start(allowed_options).await;
            let mut request = allowed_server.ws_url("/acp").into_client_request().unwrap();
            request
                .headers_mut()
                .insert(ORIGIN, "https://example.com".parse().unwrap());
            let (mut socket, response) = connect_async(request).await.unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::SWITCHING_PROTOCOLS);
            socket.close(None).await.unwrap();
        }

        #[tokio::test]
        async fn reports_agent_spawn_failure_during_initialize() {
            let missing_program = format!("/definitely-missing-acp-agent-{}", std::process::id());
            let server = TestServer::start_with_agent(
                ServeOptions::default(),
                AcpAgentConfig::new(missing_program),
            )
            .await;
            let client = reqwest::Client::new();
            let response = initialize_http(&client, &server.http_url("/acp")).await;

            assert_eq!(
                response.status(),
                reqwest::StatusCode::INTERNAL_SERVER_ERROR
            );
            assert!(
                response
                    .text()
                    .await
                    .unwrap()
                    .contains("agent closed before initialize response")
            );
        }

        #[tokio::test]
        async fn honors_custom_path_health_and_cors_options() {
            let custom_options = ServeOptions {
                path: "/rpc".to_string(),
                cors: cors_options(vec!["https://example.com".to_string()], false).unwrap(),
                health_endpoint: false,
                ..ServeOptions::default()
            };
            let server = TestServer::start(custom_options).await;
            let client = reqwest::Client::new();

            let old_path = client
                .post(server.http_url("/acp"))
                .header(CONTENT_TYPE, "application/json")
                .body(INITIALIZE_REQUEST)
                .send()
                .await
                .unwrap();
            assert_eq!(old_path.status(), reqwest::StatusCode::NOT_FOUND);

            let health = client.get(server.http_url("/health")).send().await.unwrap();
            assert_eq!(health.status(), reqwest::StatusCode::NOT_FOUND);

            let preflight = client
                .request(reqwest::Method::OPTIONS, server.http_url("/rpc"))
                .header(ORIGIN, "https://example.com")
                .header(ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .send()
                .await
                .unwrap();
            assert_eq!(preflight.status(), reqwest::StatusCode::OK);
            assert_eq!(
                preflight
                    .headers()
                    .get(ACCESS_CONTROL_ALLOW_ORIGIN)
                    .unwrap(),
                "https://example.com"
            );

            let initialized = initialize_http(&client, &server.http_url("/rpc")).await;
            assert_eq!(initialized.status(), reqwest::StatusCode::OK);
            let connection_id = initialized
                .headers()
                .get(CONNECTION_ID)
                .unwrap()
                .to_str()
                .unwrap();
            let _ = client
                .delete(server.http_url("/rpc"))
                .header(CONNECTION_ID, connection_id)
                .send()
                .await;
        }
    }
}
