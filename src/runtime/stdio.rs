use std::process::{ExitStatus, Stdio};

use anyhow::{Context, Result};
use tokio::process::Command;

use crate::registry::fetch_registry;
use crate::runtime::prepare::{CommandSpec, prepare_agent_command};
use crate::runtime::process::apply_command_spec;

/// Runs an agent with stdio connected directly to the current terminal.
///
/// Keeps the temporary extraction directory alive while the agent process runs so
/// downloaded binaries do not disappear mid-execution.
pub async fn run_agent_stdio(agent_id: &str, user_args: &[String]) -> Result<ExitStatus> {
    let registry = fetch_registry().await?;
    let agent = registry
        .get_agent(agent_id)
        .with_context(|| format!("failed to resolve agent \"{agent_id}\" from registry"))?;

    let prepared = prepare_agent_command(agent, user_args).await?;
    let _temp_dir = prepared.temp_dir;
    run_command_stdio(prepared.spec, &agent.id).await
}

async fn run_command_stdio(spec: CommandSpec, subject: &str) -> Result<ExitStatus> {
    let program_display = spec.program.to_string_lossy().into_owned();
    let mut command = Command::new(&spec.program);
    apply_command_spec(&mut command, &spec);
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    command
        .status()
        .await
        .with_context(|| format!("failed to run {program_display} for {subject}"))
}
