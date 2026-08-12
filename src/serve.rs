//! ACP HTTP/SSE and WebSocket serving for a single registry agent.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Channel, Client, ConnectTo, LineDirection, RawJsonRpcMessage, Role,
    schema::v1::{RequestId, Response as JsonRpcResponse},
};
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
use tokio::sync::mpsc;

/// ACP HTTP router configuration shared by standalone and named servers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRouterOptions {
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

impl Default for AgentRouterOptions {
    fn default() -> Self {
        Self {
            path: "/acp".to_string(),
            cors: CorsOptions::disabled(),
            health_endpoint: true,
            readyz_endpoint: true,
        }
    }
}

/// HTTP listener configuration for serving one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeOptions {
    /// Hostname or IP address to bind.
    pub host: String,
    /// TCP port to bind. Port `0` lets the operating system choose a port.
    pub port: u16,
    /// Optional URL prefix applied to all served endpoints. Defaults to the
    /// server root when `None`.
    pub subpath: Option<String>,
    /// ACP router configuration.
    pub router: AgentRouterOptions,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 0,
            subpath: None,
            router: AgentRouterOptions::default(),
        }
    }
}

/// Builds the browser-origin policy shared by standalone and named servers.
pub fn cors_options(origins: Vec<String>, allow_any: bool) -> Result<CorsOptions> {
    if allow_any && !origins.is_empty() {
        bail!("CORS origins cannot be combined with allow_any_origin");
    }
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
    let mut router = agent_router(config, &options.router)?;
    if let Some(subpath) = options.subpath.as_deref() {
        validate_subpath(subpath)?;
        router = Router::new().nest(subpath, router);
    }
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
        options.router.path
    );
    if options.router.readyz_endpoint {
        eprintln!(
            "Agent readiness probe at http://{address}{}/readyz",
            options.subpath.as_deref().unwrap_or("")
        );
    }
    serve_listener(listener, router).await
}

// Named servers reuse this unprefixed router so both serving modes keep the
// same transport, CORS, health, and readiness behavior.
pub(crate) fn agent_router(config: AcpAgentConfig, options: &AgentRouterOptions) -> Result<Router> {
    agent_router_with_stderr(config, options, AgentStderr::spawn())
}

fn agent_router_with_stderr(
    config: AcpAgentConfig,
    options: &AgentRouterOptions,
    stderr: AgentStderr,
) -> Result<Router> {
    let server_options = http_server_options(options)?;
    let health = AgentHealth::default();
    // Wrap each agent so its stderr lands in this process's logs (through the
    // non-blocking [`AgentStderr`] sink) and its launch outcome feeds
    // `GET /readyz` (see [`LaunchGuard`] for why the per-connection
    // `LaunchState` must survive library teardown).
    let agent_factory = {
        let config = config.clone();
        let health = health.clone();
        let stderr = stderr.clone();
        move || {
            let state = Arc::new(LaunchState::new(health.next_generation()));
            let callback_state = state.clone();
            let stderr = stderr.clone();
            let agent = AcpAgent::new(config.clone()).with_debug(move |line, direction| {
                forward_agent_line(line, direction, &callback_state, &stderr)
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
    Ok(router)
}

async fn serve_listener(listener: TcpListener, router: Router) -> Result<()> {
    axum::serve(listener, router)
        .await
        .context("ACP HTTP server failed")
}

pub(crate) fn validate_router_options(options: &AgentRouterOptions) -> Result<()> {
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
    Ok(())
}

fn validate_subpath(subpath: &str) -> Result<()> {
    if !subpath.starts_with('/') {
        bail!("subpath must start with '/'");
    }
    if subpath.len() == 1 {
        bail!("subpath cannot be '/'");
    }
    if subpath.ends_with('/') {
        bail!("subpath must not end with '/'");
    }

    Ok(())
}

fn http_server_options(options: &AgentRouterOptions) -> Result<ServerOptions> {
    validate_router_options(options)?;
    Ok(ServerOptions {
        path: options.path.clone(),
        cors: options.cors.clone(),
        health_endpoint: options.health_endpoint,
    })
}

// Readiness reflects the outcome of the most recent launch, not the most
// recent completion: each launch receives a monotonic generation, and an
// outcome recorded by an older generation is counted but never overwrites
// the readiness of a newer launch.
#[derive(Clone)]
struct AgentHealth {
    state: Arc<Mutex<AgentHealthState>>,
    next_generation: Arc<AtomicU64>,
}

#[derive(Default)]
struct AgentHealthState {
    attempts: u64,
    failures: u64,
    /// Generation of the launch whose outcome is reflected in
    /// `last_attempt_failed` and `last_failure`.
    outcome_generation: u64,
    last_attempt_failed: bool,
    last_failure: Option<AgentFailure>,
}

impl Default for AgentHealth {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(AgentHealthState::default())),
            next_generation: Arc::new(AtomicU64::new(0)),
        }
    }
}

#[derive(Clone)]
struct AgentFailure {
    at: SystemTime,
    detail: String,
}

impl AgentHealth {
    /// Allocates the monotonic generation for one agent launch.
    fn next_generation(&self) -> u64 {
        self.next_generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn record_ok(&self, generation: u64) {
        let mut state = self.state.lock().expect("agent health mutex poisoned");
        state.attempts += 1;
        if generation > state.outcome_generation {
            state.outcome_generation = generation;
            state.last_attempt_failed = false;
        }
    }

    fn record_failure(&self, generation: u64, detail: String) {
        let mut state = self.state.lock().expect("agent health mutex poisoned");
        state.attempts += 1;
        state.failures += 1;
        if generation > state.outcome_generation {
            state.outcome_generation = generation;
            state.last_attempt_failed = true;
            state.last_failure = Some(AgentFailure {
                at: SystemTime::now(),
                detail,
            });
        }
    }
}

// The transport may cancel the connection future before it returns. The frame
// observer records initialize outcomes immediately; the drop guard preserves
// failures that happen before a response is observed.
#[derive(Default)]
struct LaunchState {
    /// Monotonic launch order assigned at factory creation; health updates
    /// from an older generation cannot overwrite a newer launch's outcome.
    generation: u64,
    outcome_recorded: AtomicBool,
    initialize_requested: AtomicBool,
    initialize_id: Mutex<Option<RequestId>>,
    stderr_tail: Mutex<String>,
}

const STDERR_TAIL_BYTES: usize = 16 * 1024;

impl LaunchState {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            ..Self::default()
        }
    }

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

    fn initialize_requested(&self, id: RequestId) {
        self.initialize_requested.store(true, Ordering::SeqCst);
        *self
            .initialize_id
            .lock()
            .expect("initialize id mutex poisoned") = Some(id);
    }

    fn initialize_id_matches(&self, id: &RequestId) -> bool {
        self.initialize_id
            .lock()
            .expect("initialize id mutex poisoned")
            .as_ref()
            == Some(id)
    }

    fn try_record_outcome(&self) -> bool {
        self.outcome_recorded
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }
}

/// Bounded, non-blocking sink for agent stderr lines.
///
/// The agent library invokes its debug callback from Tokio worker tasks, so
/// writing to stderr there would perform blocking I/O on a worker thread and
/// contend on the global stderr lock for every line.
/// Lines are queued in a bounded channel and written by one dedicated task
/// instead, keeping worker threads free regardless of log volume.
#[derive(Clone)]
struct AgentStderr {
    tx: mpsc::Sender<String>,
    dropped: Arc<AtomicU64>,
}

/// Maximum queued stderr lines per server; excess lines are dropped.
const STDERR_CHANNEL_CAPACITY: usize = 1024;

impl AgentStderr {
    fn spawn() -> Self {
        Self::spawn_with(|line| eprintln!("[agent stderr] {line}"))
    }

    fn spawn_with(write: impl Fn(String) + Send + 'static) -> Self {
        let (tx, mut rx) = mpsc::channel(STDERR_CHANNEL_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let writer_dropped = dropped.clone();
        tokio::spawn(async move {
            let mut reported_drops = 0u64;
            let mut last_report = Instant::now();
            while let Some(line) = rx.recv().await {
                write(line);
                // Rate-limited drop metric: at most one warning per second so
                // operators see overflow without the warning itself flooding.
                let total_drops = writer_dropped.load(Ordering::Relaxed);
                if total_drops > reported_drops && last_report.elapsed() >= Duration::from_secs(1) {
                    reported_drops = total_drops;
                    last_report = Instant::now();
                    eprintln!("[agent stderr] dropped {total_drops} lines: stderr channel full");
                }
            }
        });
        Self { tx, dropped }
    }

    /// Queues one line for the writer task without blocking.
    ///
    /// A full queue drops the line and counts it instead of blocking, keeping
    /// worker-thread latency bounded by [`STDERR_CHANNEL_CAPACITY`].
    fn push(&self, line: &str) {
        if self.tx.try_send(line.to_string()).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn forward_agent_line(
    line: &str,
    direction: LineDirection,
    state: &LaunchState,
    stderr: &AgentStderr,
) {
    match direction {
        LineDirection::Stderr => {
            state.push_stderr(line);
            stderr.push(line);
        }
        LineDirection::Stdout | LineDirection::Stdin => {}
    }
}

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

// Records once from the connection result or, on cancellation, from the
// signals collected before the future was dropped.
// `complete` consumes the guard and `Drop` covers the cancellation path, so
// exactly-once recording is guaranteed by ownership without shared state.
struct LaunchGuard {
    state: Arc<LaunchState>,
    health: AgentHealth,
    completed: bool,
}

impl LaunchGuard {
    fn complete(mut self, result: &agent_client_protocol::Result<()>) {
        self.completed = true;
        if self.state.outcome_recorded.load(Ordering::SeqCst) {
            return;
        }
        let generation = self.state.generation;
        match result {
            Ok(()) => {
                let detail = if self.state.initialize_requested.load(Ordering::SeqCst) {
                    "agent connection ended before completing initialize".to_string()
                } else {
                    "agent connection ended without sending initialize".to_string()
                };
                if self.state.try_record_outcome() {
                    self.health.record_failure(generation, detail);
                }
            }
            Err(error) => {
                if self.state.try_record_outcome() {
                    self.health.record_failure(generation, error.to_string());
                }
            }
        }
    }
}

impl Drop for LaunchGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let generation = self.state.generation;
        if self.state.initialize_requested.load(Ordering::SeqCst) {
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
            if self.state.try_record_outcome() {
                self.health.record_failure(generation, detail);
            }
        } else if self.state.try_record_outcome() {
            self.health.record_failure(
                generation,
                "agent connection ended without sending initialize".to_string(),
            );
        }
    }
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
        let (agent_channel, agent_future) =
            <AcpAgent as ConnectTo<Client>>::into_channel_and_future(self.inner);
        let (client_channel, client_future) = client.into_channel_and_future();
        let state_for_requests = self.state.clone();
        let state_for_responses = self.state.clone();
        let health_for_responses = self.health.clone();
        let generation = self.state.generation;
        let bridge = Channel::bridge_with_inspection(
            agent_channel,
            client_channel,
            move |message| {
                if let RawJsonRpcMessage::Response(response) = message {
                    let id = match response {
                        JsonRpcResponse::Result { id, .. } | JsonRpcResponse::Error { id, .. } => {
                            id
                        }
                    };
                    if state_for_responses.initialize_id_matches(id)
                        && state_for_responses.try_record_outcome()
                    {
                        match response {
                            JsonRpcResponse::Result { .. } => {
                                health_for_responses.record_ok(generation);
                            }
                            JsonRpcResponse::Error { .. } => {
                                let detail = serde_json::to_string(message)
                                    .unwrap_or_else(|_| "initialize failed".to_string());
                                health_for_responses.record_failure(generation, detail);
                            }
                        }
                    }
                }
                Ok(())
            },
            move |message| {
                if let RawJsonRpcMessage::Request(request) = message
                    && request.method.as_ref() == "initialize"
                {
                    state_for_requests.initialize_requested(request.id.clone());
                }
                Ok(())
            },
        );
        let result = futures::try_join!(agent_future, client_future, bridge).map(|_| ());
        guard.complete(&result);
        result
    }
}

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
            let error = http_server_options(&AgentRouterOptions {
                path: path.to_string(),
                ..AgentRouterOptions::default()
            })
            .unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }

        for (subpath, expected) in [
            ("myapp", "must start with '/'"),
            ("/", "cannot be '/'"),
            ("/myapp/", "must not end with '/'"),
        ] {
            let error = validate_subpath(subpath).unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }

        let error = cors_options(vec!["bad\norigin".to_string()], false).unwrap_err();
        assert!(error.to_string().contains("invalid HTTP header value"));

        let error = cors_options(vec!["https://example.com".to_string()], true).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot be combined with allow_any_origin")
        );
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

    #[tokio::test]
    async fn agent_router_builds_an_unprefixed_router() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;

        let router = agent_router(
            AcpAgentConfig::new("unused-agent"),
            &AgentRouterOptions::default(),
        )
        .unwrap();

        let health = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);

        let readyz = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(readyz.status(), StatusCode::OK);

        let nested_health = router
            .oneshot(
                Request::builder()
                    .uri("/agent/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(nested_health.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn agent_router_validates_router_options() {
        let options = AgentRouterOptions {
            path: "acp".to_string(),
            ..AgentRouterOptions::default()
        };

        let error = agent_router(AcpAgentConfig::new("unused-agent"), &options)
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("ACP endpoint path must start with '/'")
        );
    }

    #[test]
    fn launch_guard_records_failure_for_clean_connection_exit() {
        let state = Arc::new(LaunchState::new(1));
        let health = AgentHealth::default();
        let guard = LaunchGuard {
            state: state.clone(),
            health: health.clone(),
            completed: false,
        };

        guard.complete(&Ok(()));
        // The consumed guard is dropped at the end of this scope; it must not
        // record a second outcome.

        let recorded = health.state.lock().unwrap();
        assert_eq!(recorded.attempts, 1);
        assert_eq!(recorded.failures, 1);
        assert!(recorded.last_attempt_failed);
    }

    #[test]
    fn launch_guard_drop_records_success_after_protocol_response() {
        let state = Arc::new(LaunchState::new(1));
        let health = AgentHealth::default();
        state.outcome_recorded.store(true, Ordering::SeqCst);
        health.record_ok(1);
        {
            let _guard = LaunchGuard {
                state: state.clone(),
                health: health.clone(),
                completed: false,
            };
        }

        let recorded = health.state.lock().unwrap();
        assert_eq!(recorded.attempts, 1);
        assert_eq!(recorded.failures, 0);
        assert!(!recorded.last_attempt_failed);
    }

    #[test]
    fn launch_guard_drop_records_failure_with_stderr_tail_after_initialize() {
        let state = Arc::new(LaunchState::new(1));
        state.initialize_requested.store(true, Ordering::SeqCst);
        state.push_stderr("error: boom");
        let health = AgentHealth::default();
        {
            let _guard = LaunchGuard {
                state: state.clone(),
                health: health.clone(),
                completed: false,
            };
        }

        let recorded = health.state.lock().unwrap();
        assert_eq!(recorded.attempts, 1);
        assert_eq!(recorded.failures, 1);
        assert!(recorded.last_attempt_failed);
        assert!(
            recorded
                .last_failure
                .as_ref()
                .unwrap()
                .detail
                .contains("error: boom")
        );
    }

    #[test]
    fn launch_guard_drop_without_signals_records_failure() {
        let state = Arc::new(LaunchState::new(1));
        let health = AgentHealth::default();
        {
            let _guard = LaunchGuard {
                state: state.clone(),
                health: health.clone(),
                completed: false,
            };
        }

        let recorded = health.state.lock().unwrap();
        assert_eq!(recorded.attempts, 1);
        assert_eq!(recorded.failures, 1);
        assert!(recorded.last_attempt_failed);
    }

    #[test]
    fn stale_launch_outcome_does_not_overwrite_newer_generation() {
        let health = AgentHealth::default();
        // Launch 1 and launch 2 started concurrently; launch 2 (newer) failed
        // first, then launch 1's connection closed later reporting success.
        health.record_failure(2, "launch 2 failed".to_string());
        health.record_ok(1);

        let recorded = health.state.lock().unwrap();
        assert_eq!(recorded.attempts, 2);
        assert_eq!(recorded.failures, 1);
        assert!(recorded.last_attempt_failed);
        assert!(
            recorded
                .last_failure
                .as_ref()
                .unwrap()
                .detail
                .contains("launch 2 failed")
        );
    }

    #[test]
    fn newer_launch_outcome_overwrites_older_generation() {
        let health = AgentHealth::default();
        health.record_failure(1, "launch 1 failed".to_string());
        health.record_ok(2);

        let recorded = health.state.lock().unwrap();
        assert_eq!(recorded.attempts, 2);
        assert_eq!(recorded.failures, 1);
        assert!(!recorded.last_attempt_failed);
        // The older failure detail stays available for diagnostics.
        assert!(
            recorded
                .last_failure
                .as_ref()
                .unwrap()
                .detail
                .contains("launch 1 failed")
        );
    }

    #[test]
    fn generations_are_monotonic_across_launches() {
        let health = AgentHealth::default();
        assert_eq!(health.next_generation(), 1);
        assert_eq!(health.next_generation(), 2);
        assert_eq!(health.next_generation(), 3);
    }

    #[cfg(unix)]
    mod network {
        use std::net::SocketAddr;
        use std::time::{Duration, Instant};

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
                Self::start_with_stderr_sink(options, config, AgentStderr::spawn()).await
            }

            async fn start_with_stderr_sink(
                options: ServeOptions,
                config: AcpAgentConfig,
                stderr: AgentStderr,
            ) -> Self {
                let listener = TcpListener::bind((options.host.as_str(), options.port))
                    .await
                    .unwrap();
                let address = listener.local_addr().unwrap();
                let mut router = agent_router_with_stderr(config, &options.router, stderr).unwrap();
                if let Some(subpath) = options.subpath.as_deref() {
                    validate_subpath(subpath).unwrap();
                    router = Router::new().nest(subpath, router);
                }
                let task = tokio::spawn(async move {
                    serve_listener(listener, router).await.unwrap();
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
                router: AgentRouterOptions {
                    cors: cors_options(Vec::new(), true).unwrap(),
                    ..AgentRouterOptions::default()
                },
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

            let readyz = readyz_until_failure(&client, &server).await;
            assert!(readyz.contains("1 of 1 agent launches failed"));
            assert!(
                readyz.contains("No such file or directory"),
                "readyz should include the spawn failure cause: {readyz}"
            );
        }

        #[tokio::test]
        async fn readyz_surfaces_agent_stderr_tail_after_exit_failure() {
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

        #[tokio::test(flavor = "multi_thread")]
        async fn health_stays_responsive_while_agent_floods_stderr() {
            let stderr = AgentStderr::spawn_with(|_| std::thread::sleep(Duration::from_millis(5)));
            let server = TestServer::start_with_stderr_sink(
                ServeOptions::default(),
                AcpAgentConfig::new("/bin/sh").args([
                    "-c",
                    r#"while IFS= read -r line; do
i=0
while [ $i -lt 5000 ]; do echo "stderr noise $i" >&2; i=$((i+1)); done
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}'
done"#,
                ]),
                stderr.clone(),
            )
            .await;
            let client = reqwest::Client::new();

            // The WebSocket handshake spawns the agent; the initialize request
            // makes it flood 5000 stderr lines into the bounded sink.
            let (mut socket, _) = connect_async(server.ws_url("/acp")).await.unwrap();
            socket
                .send(Message::Text(INITIALIZE_REQUEST.into()))
                .await
                .unwrap();
            let frame = timeout(Duration::from_secs(5), socket.next())
                .await
                .expect("initialize timed out")
                .unwrap()
                .unwrap();
            let Message::Text(_) = frame else {
                panic!("expected text response, got {frame:?}");
            };

            // The 5ms-per-line sink takes ~25s to drain the flood, so the
            // channel stays full; health must remain responsive throughout.
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut saw_drops = false;
            while Instant::now() < deadline {
                let response = timeout(
                    Duration::from_millis(500),
                    client.get(server.http_url("/health")).send(),
                )
                .await
                .expect("health request stalled behind agent stderr")
                .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                if stderr.dropped.load(Ordering::Relaxed) > 0 {
                    saw_drops = true;
                    break;
                }
                sleep(Duration::from_millis(20)).await;
            }
            assert!(saw_drops, "agent stderr never overflowed the channel");
            socket.close(None).await.unwrap();
        }

        #[tokio::test]
        async fn honors_custom_path_health_and_cors_options() {
            let custom_options = ServeOptions {
                router: AgentRouterOptions {
                    path: "/rpc".to_string(),
                    cors: cors_options(vec!["https://example.com".to_string()], false).unwrap(),
                    health_endpoint: false,
                    ..AgentRouterOptions::default()
                },
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
