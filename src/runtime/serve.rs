use anyhow::{Context, Result};
use clap::ValueEnum;
use std::process::ExitStatus;

use crate::registry::fetch_registry;
use crate::runtime::prepare::prepare_agent_command;
use crate::runtime::transports::{h2, tcp, ws};

/// Indicates which transport protocol will expose the agent's STDIO streams.
///
/// * `Http` stands for the HTTP/2 full-duplex stream transport exposed by `h2`.
/// * `Tcp` exposes raw bytes over a single TCP connection (no framing or RPC).
/// * `Ws` publishes a JSON-RPC over WebSocket wrapper used by the existing client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ServeTransport {
    /// HTTP/2 stream transport implemented by [`crate::runtime::transports::h2`].
    Http,
    /// Raw TCP byte-stream transport implemented by [`crate::runtime::transports::tcp`].
    Tcp,
    /// WebSocket + JSON-RPC transport implemented by [`crate::runtime::transports::ws`].
    Ws,
}

/// Runtime options shared by all transport implementations.
///
/// `host`/`port` determine the bound listener address, while `transport` picks the
/// serialization/multiplexing layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeOptions {
    /// Transport protocol that will expose the child process.
    pub transport: ServeTransport,
    /// Listener host or IP address to bind.
    pub host: String,
    /// Listener port. `0` lets the OS choose an ephemeral port.
    pub port: u16,
}

/// Serves an ACP agent via the chosen transport so external clients can interact.
///
/// Fetches the registry entry, prepares the agent command (downloading binaries if
/// needed), and then dispatches to the transport module that knows how to wire
/// stdio across TCP, HTTP/2, or WebSocket.
pub async fn serve_agent(
    agent_id: &str,
    options: ServeOptions,
    user_args: &[String],
) -> Result<ExitStatus> {
    let registry = fetch_registry().await?;
    let agent = registry
        .get_agent(agent_id)
        .with_context(|| format!("failed to resolve agent \"{agent_id}\" from registry"))?;

    let prepared = prepare_agent_command(agent, user_args).await?;

    match options.transport {
        ServeTransport::Http => {
            h2::serve_h2(prepared, &agent.id, &options.host, options.port).await
        }
        ServeTransport::Tcp => {
            tcp::serve_tcp(prepared, &agent.id, &options.host, options.port).await
        }
        ServeTransport::Ws => ws::serve_ws(prepared, &agent.id, &options.host, options.port).await,
    }
}
