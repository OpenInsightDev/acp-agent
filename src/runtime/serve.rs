use anyhow::{Context, Result};
use clap::ValueEnum;
use std::process::ExitStatus;

use crate::registry::fetch_registry;
use crate::runtime::prepare::prepare_agent_command;
use crate::runtime::transports::{h2, tcp, ws};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ServeTransport {
    Http,
    Tcp,
    Ws,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeOptions {
    pub transport: ServeTransport,
    pub host: String,
    pub port: u16,
}

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
