use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use agent_client_protocol::{AcpAgent, AcpAgentConfig, Client, ConnectTo, LineDirection, Role};
use agent_client_protocol_http::{AcpHttpServer, CorsOptions, ServerOptions};
use anyhow::{Context, Result, bail};
use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use tokio::net::TcpListener;

/// HTTP listener and ACP endpoint configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeOptions {
    /// Hostname or IP address to bind.
    pub host: String,
    /// TCP port to bind. Port `0` lets the operating system choose a port.
    pub port: u16,
    /// Optional URL prefix applied to all served endpoints (ACP, health,
    /// readyz). Defaults to the server root when `None`.
    pub subpath: Option<String>,
    /// Path serving ACP over HTTP/SSE and WebSocket.
    pub path: String,
    /// Cross-origin browser access policy.
    pub cors: CorsOptions,
    /// Whether to expose `GET /health`.
    pub health_endpoint: bool,
    /// Whether to expose `GET /readyz` with agent launch health.
    ///
    /// `GET /health` stays `ok` even while agent launches fail, so this probe
    /// exists for operators/orchestrators to see agent-process health.
    pub readyz_endpoint: bool,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 0,
            subpath: None,
            path: "/acp".to_string(),
            cors: CorsOptions::disabled(),
            health_endpoint: true,
            readyz_endpoint: true,
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
        "Serving ACP agent at http://{address}{}{} (WebSocket available on the same endpoint)",
        options.subpath.as_deref().unwrap_or(""),
        options.path
    );
    if options.readyz_endpoint {
        eprintln!(
            "Agent readiness probe at http://{address}{}/readyz",
            options.subpath.as_deref().unwrap_or("")
        );
    }
    serve_listener(listener, config, options, server_options).await
}

async fn serve_listener(
    listener: TcpListener,
    config: AcpAgentConfig,
    options: ServeOptions,
    server_options: ServerOptions,
) -> Result<()> {
    let health = AgentHealth::default();
    // Wrap each agent so its stderr lands in this process's logs and its
    // launch outcome feeds `GET /readyz` (see [`LaunchGuard`] for why the
    // per-connection `LaunchState` must survive library teardown).
    let agent_factory = {
        let config = config.clone();
        let health = health.clone();
        move || {
            let state = Arc::new(LaunchState::default());
            let callback_state = state.clone();
            let agent = AcpAgent::new(config.clone()).with_debug(move |line, direction| {
                forward_agent_line(line, direction, &callback_state)
            });
            ObservedAgent::new(agent, health.clone(), state)
        }
    };

    let mut router = AcpHttpServer::new(agent_factory)
        .with_options(server_options)
        .into_router();
    // `/readyz` is added after `into_router()` so it stays outside the CORS
    // layer (like the library's own `/health`): probes must stay reachable
    // regardless of CORS policy.
    if options.readyz_endpoint {
        router = router.route("/readyz", get(readyz).with_state(health));
    }
    // When a `--subpath` is configured, serve the entire tree (ACP endpoint,
    // health, readyz) under that URL prefix.
    if let Some(subpath) = options.subpath.as_deref() {
        router = Router::new().nest(subpath, router);
    }

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
    if options.readyz_endpoint && options.path == "/readyz" {
        bail!("ACP endpoint path conflicts with the readiness endpoint");
    }
    if let Some(subpath) = &options.subpath {
        if !subpath.starts_with('/') {
            bail!("subpath must start with '/'");
        }
        if subpath.len() == 1 {
            bail!("subpath cannot be '/'");
        }
        if subpath.ends_with('/') {
            bail!("subpath must not end with '/'");
        }
    }

    Ok(ServerOptions {
        path: options.path.clone(),
        cors: options.cors.clone(),
        health_endpoint: options.health_endpoint,
    })
}

/// Per-server launch-outcome tracking for `GET /readyz`.
///
/// Readiness follows the *most recent* launch, not any historical failure:
/// one transient failure must not flip the probe permanently, and a later
/// success clears it (the last failure detail is kept for diagnostics).
#[derive(Clone, Default)]
struct AgentHealth {
    state: Arc<Mutex<AgentHealthState>>,
}

#[derive(Default)]
struct AgentHealthState {
    attempts: u64,
    failures: u64,
    last_attempt_failed: bool,
    last_failure: Option<AgentFailure>,
}

/// Detail of the most recent failed agent launch.
#[derive(Clone)]
struct AgentFailure {
    at: SystemTime,
    detail: String,
}

impl AgentHealth {
    fn record_ok(&self) {
        let mut state = self.state.lock().expect("agent health mutex poisoned");
        state.attempts += 1;
        state.last_attempt_failed = false;
    }

    fn record_failure(&self, detail: String) {
        let mut state = self.state.lock().expect("agent health mutex poisoned");
        state.attempts += 1;
        state.failures += 1;
        state.last_attempt_failed = true;
        state.last_failure = Some(AgentFailure {
            at: SystemTime::now(),
            detail,
        });
    }
}

/// Per-connection launch signals shared between the debug callback and the
/// connection future.
///
/// The library tears a connection down by aborting its agent task, which
/// cancels the in-flight connection future before it reports its outcome
/// (observed for ~80% of fast agent-exit failures). Signals recorded from the
/// debug callback (which always fires) plus a drop guard on the future make
/// the outcome observable even when the future is cancelled.
#[derive(Default)]
struct LaunchState {
    /// Agent stdout carried a JSON-RPC response. Stderr is not a liveness
    /// signal — healthy agents write stderr too.
    protocol_responded: AtomicBool,
    /// An initialize request was sent; failures are only recorded when this
    /// is set, so probe connections that never initialize don't count.
    initialize_requested: AtomicBool,
    /// Bounded tail of agent stderr, used as failure diagnostics.
    stderr_tail: Mutex<String>,
    /// Ensures the outcome is recorded only once (`complete()` and the drop
    /// guard may both fire for the same launch).
    outcome_recorded: AtomicBool,
}

/// Maximum stderr retained per connection for `GET /readyz` diagnostics.
const STDERR_TAIL_BYTES: usize = 16 * 1024;

impl LaunchState {
    fn push_stderr(&self, line: &str) {
        let mut tail = self.stderr_tail.lock().expect("stderr tail mutex poisoned");
        tail.push_str(line);
        tail.push('\n');
        if tail.len() > STDERR_TAIL_BYTES {
            let start = tail.len() - STDERR_TAIL_BYTES;
            *tail = tail.split_off(start);
            if let Some(newline) = tail.find('\n') {
                tail.drain(..=newline);
            }
        }
    }
}

/// Forwards agent stderr to this process's logs and records the launch
/// signals used by `GET /readyz`.
fn forward_agent_line(line: &str, direction: LineDirection, state: &LaunchState) {
    match direction {
        LineDirection::Stderr => {
            eprintln!("[agent stderr] {line}");
            state.push_stderr(line);
        }
        LineDirection::Stdout => {
            if is_jsonrpc_response(line) {
                state.protocol_responded.store(true, Ordering::SeqCst);
            }
        }
        LineDirection::Stdin => {
            if line.contains("\"method\":\"initialize\"") {
                state.initialize_requested.store(true, Ordering::SeqCst);
            }
        }
    }
}

/// Whether a stdio line is a JSON-RPC response (top-level `result`/`error`),
/// which proves the agent process is alive and responsive.
fn is_jsonrpc_response(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|value| {
            value
                .as_object()
                .map(|object| object.contains_key("result") || object.contains_key("error"))
        })
        .unwrap_or(false)
}

/// An [`AcpAgent`] whose launch outcome is recorded in [`AgentHealth`].
struct ObservedAgent {
    inner: AcpAgent,
    health: AgentHealth,
    state: Arc<LaunchState>,
}

impl ObservedAgent {
    fn new(inner: AcpAgent, health: AgentHealth, state: Arc<LaunchState>) -> Self {
        Self {
            inner,
            health,
            state,
        }
    }
}

/// Records a launch outcome exactly once — from the connection result, or,
/// when the future is cancelled by teardown, from a drop guard using the
/// observed signals: responded → success; initialize sent but no response →
/// failure with the stderr tail; neither → client probe, record nothing.
struct LaunchGuard {
    state: Arc<LaunchState>,
    health: AgentHealth,
    completed: bool,
}

impl LaunchGuard {
    fn complete(mut self, result: &agent_client_protocol::Result<()>) {
        self.completed = true;
        match result {
            Ok(()) => self.record(Outcome::Success),
            Err(error) => self.record(Outcome::Failure(error.to_string())),
        }
    }

    fn record(&self, outcome: Outcome) {
        if self.state.outcome_recorded.swap(true, Ordering::SeqCst) {
            return;
        }
        match outcome {
            Outcome::Success => self.health.record_ok(),
            Outcome::Failure(detail) => self.health.record_failure(detail),
        }
    }
}

impl Drop for LaunchGuard {
    fn drop(&mut self) {
        if self.completed || self.state.outcome_recorded.load(Ordering::SeqCst) {
            return;
        }
        if self.state.protocol_responded.load(Ordering::SeqCst) {
            self.record(Outcome::Success);
        } else if self.state.initialize_requested.load(Ordering::SeqCst) {
            let tail = self
                .state
                .stderr_tail
                .lock()
                .expect("stderr tail mutex poisoned");
            let detail = if tail.is_empty() {
                "agent connection ended before completing initialize (no stderr captured)"
                    .to_string()
            } else {
                format!("agent connection ended before completing initialize; stderr tail:\n{tail}")
            };
            self.record(Outcome::Failure(detail));
        }
    }
}

enum Outcome {
    Success,
    Failure(String),
}

impl ConnectTo<Client> for ObservedAgent {
    async fn connect_to(
        self,
        client: impl ConnectTo<<Client as Role>::Counterpart>,
    ) -> agent_client_protocol::Result<()> {
        let guard = LaunchGuard {
            state: self.state.clone(),
            health: self.health.clone(),
            completed: false,
        };
        let result = <AcpAgent as ConnectTo<Client>>::connect_to(self.inner, client).await;
        guard.complete(&result);
        result
    }
}

/// `GET /readyz` handler.
///
/// `200 ready` while the most recent agent launch succeeded, otherwise `503`
/// with the failure counts and the last failure detail (including the agent
/// stderr tail). Unlike `GET /health` (HTTP-server liveness), this reflects
/// agent-process health.
async fn readyz(State(health): State<AgentHealth>) -> Response {
    let (attempts, failures, last_attempt_failed, last_failure) = {
        let state = health.state.lock().expect("agent health mutex poisoned");
        (
            state.attempts,
            state.failures,
            state.last_attempt_failed,
            state.last_failure.clone(),
        )
    };

    if !last_attempt_failed {
        return (StatusCode::OK, "ready\n").into_response();
    }

    let detail = last_failure
        .map(|failure| {
            let age = failure
                .at
                .elapsed()
                .map(|elapsed| format!("{elapsed:?} ago"))
                .unwrap_or_else(|_| "recently".to_string());
            format!("last failure ({age}): {}\n", failure.detail)
        })
        .unwrap_or_default();
    (
        StatusCode::SERVICE_UNAVAILABLE,
        format!("not ready: {failures} of {attempts} agent launches failed; {detail}"),
    )
        .into_response()
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
            ("/readyz", "conflicts with the readiness endpoint"),
        ] {
            let error = http_server_options(&ServeOptions {
                path: path.to_string(),
                ..ServeOptions::default()
            })
            .unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }

        for (subpath, expected) in [
            ("myapp", "must start with '/'"),
            ("/", "cannot be '/'"),
            ("/myapp/", "must not end with '/'"),
        ] {
            let error = http_server_options(&ServeOptions {
                subpath: Some(subpath.to_string()),
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
        use tokio::time::{sleep, timeout};

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
                    serve_listener(listener, config, options, server_options)
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

            let readyz = client.get(server.http_url("/readyz")).send().await.unwrap();
            assert_eq!(readyz.status(), reqwest::StatusCode::OK);
            assert_eq!(readyz.text().await.unwrap(), "ready\n");

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

            // The readiness probe must surface the launch failure with its
            // cause, unlike the liveness probe which only reflects the HTTP
            // server. The outcome is recorded asynchronously, so poll briefly.
            let readyz = readyz_until_failure(&client, &server).await;
            assert!(readyz.contains("1 of 1 agent launches failed"));
            assert!(
                readyz.contains("No such file or directory"),
                "readyz should include the spawn failure cause: {readyz}"
            );
        }

        #[tokio::test]
        async fn readyz_surfaces_agent_stderr_tail_after_exit_failure() {
            // Mirrors the real-world failure mode where the agent process
            // starts, writes its startup error to stderr (e.g. Deno's
            // dependency-age rejection), and exits before initializing.
            let server = TestServer::start_with_agent(
                ServeOptions::default(),
                AcpAgentConfig::new("/bin/sh").args([
                    "-c",
                    "echo 'error: Could not find npm package matching version' >&2; exit 1",
                ]),
            )
            .await;
            let client = reqwest::Client::new();
            let response = initialize_http(&client, &server.http_url("/acp")).await;

            assert_eq!(
                response.status(),
                reqwest::StatusCode::INTERNAL_SERVER_ERROR
            );

            let readyz = readyz_until_failure(&client, &server).await;
            assert!(
                readyz.contains("Could not find npm package matching version"),
                "readyz should include the agent stderr tail: {readyz}"
            );
        }

        /// Polls `GET /readyz` until it reports the launch failure, returning
        /// the response body.
        async fn readyz_until_failure(client: &reqwest::Client, server: &TestServer) -> String {
            timeout(Duration::from_secs(5), async {
                loop {
                    let response = client.get(server.http_url("/readyz")).send().await.unwrap();
                    if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE {
                        return response.text().await.unwrap();
                    }
                    sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("readyz should report the agent launch failure")
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

        #[tokio::test]
        async fn serves_all_endpoints_under_the_configured_subpath() {
            let custom_options = ServeOptions {
                subpath: Some("/myapp".to_string()),
                ..ServeOptions::default()
            };
            let server = TestServer::start(custom_options).await;
            let client = reqwest::Client::new();

            // Endpoints without the subpath prefix must not be reachable.
            let bare_health = client.get(server.http_url("/health")).send().await.unwrap();
            assert_eq!(
                bare_health.status(),
                reqwest::StatusCode::NOT_FOUND,
                "health should only be under the subpath"
            );
            let bare_acp = client
                .post(server.http_url("/acp"))
                .header(CONTENT_TYPE, "application/json")
                .body(INITIALIZE_REQUEST)
                .send()
                .await
                .unwrap();
            assert_eq!(
                bare_acp.status(),
                reqwest::StatusCode::NOT_FOUND,
                "ACP should only be under the subpath"
            );

            // Health and readyz are reachable under the subpath.
            let health = client
                .get(server.http_url("/myapp/health"))
                .send()
                .await
                .unwrap();
            assert_eq!(health.status(), reqwest::StatusCode::OK);
            assert_eq!(health.text().await.unwrap(), "ok");
            let readyz = client
                .get(server.http_url("/myapp/readyz"))
                .send()
                .await
                .unwrap();
            assert_eq!(readyz.status(), reqwest::StatusCode::OK);

            // The ACP endpoint is served under the subpath.
            let initialized = initialize_http(&client, &server.http_url("/myapp/acp")).await;
            assert_eq!(initialized.status(), reqwest::StatusCode::OK);
            let connection_id = initialized
                .headers()
                .get(CONNECTION_ID)
                .unwrap()
                .to_str()
                .unwrap();
            let _ = client
                .delete(server.http_url("/myapp/acp"))
                .header(CONNECTION_ID, connection_id)
                .send()
                .await;

            // WebSocket is served under the same subpath prefix.
            let (mut socket, response) = connect_async(server.ws_url("/myapp/acp")).await.unwrap();
            assert!(
                response.headers().contains_key(CONNECTION_ID),
                "WebSocket handshake should succeed under the subpath"
            );
            socket
                .send(Message::Text(INITIALIZE_REQUEST.into()))
                .await
                .unwrap();
            let frame = timeout(Duration::from_secs(5), socket.next())
                .await
                .expect("WebSocket initialize under subpath timed out")
                .unwrap()
                .unwrap();
            let Message::Text(text) = frame else {
                panic!("expected text response, got {frame:?}");
            };
            let response: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(response["result"]["protocolVersion"], json!(1));
            socket.close(None).await.unwrap();
        }
    }
}
