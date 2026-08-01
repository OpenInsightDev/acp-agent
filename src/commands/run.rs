use std::process::ExitStatus;

use anyhow::Result;

/// Runs an ACP agent locally with terminal standard streams attached.
pub async fn run_agent(agent_id: &str, args: &[String]) -> Result<ExitStatus> {
    crate::runner::run_agent(agent_id, args).await
}
