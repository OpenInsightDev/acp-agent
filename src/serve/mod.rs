//! ACP HTTP/SSE and WebSocket serving for a single registry agent.

use agent_client_protocol_http::CorsOptions;
use anyhow::{Context, Result, bail};
use axum::Router;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::watch;

mod runtime;
mod transport;

#[cfg(test)]
pub(crate) use runtime::ReadinessFailure;
pub(crate) use runtime::{ReadinessSnapshot, RouteRuntime};
pub(crate) use transport::{
    await_termination_signal, serve_listener, serve_with_shutdown, wait_for_shutdown,
};

/// ACP route configuration shared by standalone and named servers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RouteConfig {
    /// Path serving ACP over HTTP/SSE and WebSocket.
    pub path: String,
    /// Browser origins allowed to access the route.
    pub cors_origins: Vec<String>,
    /// Whether all browser origins are accepted.
    pub allow_any_origin: bool,
    /// Whether to expose `GET /health`.
    pub health_endpoint: bool,
    /// Whether to expose `GET /readyz` with agent launch health.
    ///
    /// `GET /health` stays `ok` even while agent launches fail, so this probe
    /// exists for operators/orchestrators to see agent-process health.
    pub readyz_endpoint: bool,
    /// Maximum number of concurrent agent processes for this route.
    pub max_processes: usize,
}

/// Default maximum number of concurrent agent processes per served route.
pub const DEFAULT_MAX_PROCESSES: usize = 16;

impl Default for RouteConfig {
    fn default() -> Self {
        Self {
            path: "/acp".to_string(),
            cors_origins: Vec::new(),
            allow_any_origin: false,
            health_endpoint: true,
            readyz_endpoint: true,
            max_processes: DEFAULT_MAX_PROCESSES,
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
    /// Optional mount prefix applied to all served endpoints.
    pub mount_path: Option<String>,
    /// ACP route configuration.
    pub route: RouteConfig,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 0,
            mount_path: None,
            route: RouteConfig::default(),
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
    let resolved = crate::runner::resolve_agent_config(agent_id, args).await?;
    serve_config(resolved, options).await
}

async fn serve_config(
    resolved: crate::runner::ResolvedAgentConfig,
    options: ServeOptions,
) -> Result<()> {
    let (cancel, cancel_rx) = watch::channel(false);
    let runtime = RouteRuntime::new(resolved, &options.route, cancel_rx)?;
    let mut router = runtime.router();
    if let Some(mount_path) = options.mount_path.as_deref() {
        validate_mount_path(mount_path)?;
        router = Router::new().nest(mount_path, router);
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
        options.mount_path.as_deref().unwrap_or(""),
        options.route.path
    );
    if options.route.readyz_endpoint {
        eprintln!(
            "Agent readiness probe at http://{address}{}/readyz",
            options.mount_path.as_deref().unwrap_or("")
        );
    }
    serve_listener(listener, router, cancel).await
}

pub(crate) fn validate_route_config(options: &RouteConfig) -> Result<()> {
    if options.max_processes == 0 {
        bail!("max_processes must be greater than zero");
    }
    if options.max_processes > tokio::sync::Semaphore::MAX_PERMITS {
        bail!(
            "max_processes must not exceed {}",
            tokio::sync::Semaphore::MAX_PERMITS
        );
    }
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

fn validate_mount_path(mount_path: &str) -> Result<()> {
    if !mount_path.starts_with('/') {
        bail!("mount path must start with '/'");
    }
    if mount_path.len() == 1 {
        bail!("mount path cannot be '/'");
    }
    if mount_path.ends_with('/') {
        bail!("mount path must not end with '/'");
    }
    Ok(())
}

fn http_server_options(options: &RouteConfig) -> Result<agent_client_protocol_http::ServerOptions> {
    validate_route_config(options)?;
    Ok(agent_client_protocol_http::ServerOptions {
        path: options.path.clone(),
        cors: cors_options(options.cors_origins.clone(), options.allow_any_origin)?,
        health_endpoint: options.health_endpoint,
    })
}
