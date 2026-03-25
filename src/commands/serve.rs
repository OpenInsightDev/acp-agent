use std::process::ExitStatus;

use anyhow::Result;

use crate::runtime::serve::{ServeOptions, ServeTransport, serve_agent as runtime_serve_agent};

/// Binds an ACP agent to the requested transport/host/port, matching `serve`.
///
/// The function returns the `ExitStatus` of the spawned agent so the caller can
/// surface failures that happen while the agent is running under a network
/// transport.
pub async fn serve_agent(
    agent_id: &str,
    transport: ServeTransport,
    host: String,
    port: u16,
    args: &[String],
) -> Result<ExitStatus> {
    runtime_serve_agent(
        agent_id,
        ServeOptions {
            transport,
            host,
            port,
        },
        args,
    )
    .await
}
