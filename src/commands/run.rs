use std::process::ExitStatus;

use anyhow::Result;

use crate::runtime::stdio;

/// Runs the named agent over stdio, mirroring the CLI `run` command.
///
/// Arguments are forwarded to the agent process, and the caller receives the exit
/// status so they can decide how to report success or failure.
pub async fn run_agent(agent_id: &str, user_args: &[String]) -> Result<ExitStatus> {
    stdio::run_agent_stdio(agent_id, user_args).await
}
