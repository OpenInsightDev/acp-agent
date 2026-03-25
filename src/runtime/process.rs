use std::process::{ExitStatus, Stdio};

use anyhow::{Context, Result};
use tokio::process::{Child, Command};

use crate::runtime::prepare::CommandSpec;

/// Applies a `CommandSpec` to a `tokio::process::Command` without spawning it.
///
/// This configures the argument list, working directory (if set), and environment
/// overrides so that callers can pipe stdio or set up transports before
/// launching the process.
pub fn apply_command_spec(command: &mut Command, spec: &CommandSpec) {
    command.args(&spec.args);

    if let Some(current_dir) = &spec.current_dir {
        command.current_dir(current_dir);
    }

    for (key, value) in &spec.env {
        command.env(key, value);
    }
}

/// Spawns a child process with stdin/stdout piped and stderr inherited.
///
/// The `subject` is used solely for contextual error messages when spawning
/// fails. The caller is responsible for wiring the pipes before awaiting the
/// child.
pub fn spawn_stream_child(spec: &CommandSpec, subject: &str) -> Result<Child> {
    let program_display = spec.program.to_string_lossy().into_owned();
    let mut command = Command::new(&spec.program);
    apply_command_spec(&mut command, spec);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    command
        .spawn()
        .with_context(|| format!("failed to spawn {program_display} for {subject}"))
}

/// Terminates a child process, returning its exit status.
///
/// First attempts a non-blocking `try_wait`; if the process is still running
/// it first sends `kill` and then waits for shutdown, surfacing any errors
/// from the runtime.
pub async fn terminate_child(child: &mut Child) -> Result<ExitStatus> {
    if let Some(status) = child
        .try_wait()
        .context("failed while checking child process status")?
    {
        return Ok(status);
    }

    child
        .kill()
        .await
        .context("failed to terminate child process")?;
    child
        .wait()
        .await
        .context("failed while waiting on child process")
}
