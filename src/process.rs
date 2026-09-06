//! Local child-process helpers.

use std::process::{ExitStatus, Output, Stdio};

use tokio::process::Command;

/// Configures a child so dropping its wait future terminates that child.
pub(crate) fn cancellable(command: &mut Command) -> &mut Command {
    command.kill_on_drop(true)
}

/// Waits for a command with captured output.
pub(crate) async fn output(command: &mut Command) -> std::io::Result<Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cancellable(command).spawn()?;
    child.wait_with_output().await
}

/// Waits for a command with inherited streams.
pub(crate) async fn status(command: &mut Command) -> std::io::Result<ExitStatus> {
    let mut child = cancellable(command).spawn()?;
    child.wait().await
}
