use std::io::Write;

use anyhow::Result;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

use crate::installer::environment::{
    EnvironmentReport, InstallationPlan, InstallationResult, ToolAvailability,
};

pub(super) fn write_detection_report<W: Write>(
    writer: &mut W,
    report: &EnvironmentReport,
) -> Result<()> {
    writeln!(writer, "Environment detection results:")?;
    writeln!(writer, "JavaScript tools:")?;
    write_tool_group(writer, &report.js)?;
    writeln!(writer, "Python tools:")?;
    write_tool_group(writer, &report.python)?;
    Ok(())
}

pub(super) fn write_installation_plan<W: Write>(
    writer: &mut W,
    plan: &InstallationPlan,
) -> Result<()> {
    writeln!(writer)?;
    writeln!(writer, "Planned installation:")?;
    for target in &plan.targets {
        writeln!(
            writer,
            "{}: {}",
            target.label(),
            target.official_command_display()
        )?;
    }
    Ok(())
}

/// Asks the user to confirm installation and returns their decision.
///
/// Reads stdin through `tokio::io::stdin`, which performs the blocking read
/// on a dedicated blocking thread, so no Tokio worker waits on user input.
pub(super) async fn confirm_installation<W: Write>(writer: &mut W) -> Result<bool> {
    prompt_for_installation(&mut tokio::io::BufReader::new(tokio::io::stdin()), writer).await
}

pub(super) fn write_cancelled<W: Write>(writer: &mut W) -> Result<()> {
    writeln!(writer, "Installation cancelled.")?;
    Ok(())
}

pub(super) fn write_nothing_to_install<W: Write>(writer: &mut W) -> Result<()> {
    writeln!(writer)?;
    writeln!(
        writer,
        "Environment already satisfies the requirements. No installation is needed."
    )?;
    Ok(())
}

pub(super) fn write_installation_start<W: Write>(writer: &mut W) -> Result<()> {
    writeln!(writer)?;
    writeln!(writer, "Starting installation...")?;
    Ok(())
}

pub(super) fn write_installation_complete<W: Write>(
    writer: &mut W,
    results: &[InstallationResult],
) -> Result<()> {
    writeln!(writer)?;
    for result in results {
        writeln!(
            writer,
            "{} installed and verified at {}",
            result.target.label(),
            result.path.display()
        )?;
        if !result.on_path {
            writeln!(
                writer,
                "Note: {} was installed outside the current PATH. Open a new shell if the command is not yet recognized.",
                result.target.program()
            )?;
        }
    }
    writeln!(writer, "Environment installation complete.")?;
    Ok(())
}

fn write_tool_group<W: Write>(writer: &mut W, tools: &[ToolAvailability]) -> Result<()> {
    for tool in tools {
        match &tool.path {
            Some(path) => writeln!(writer, "{}: available ({})", tool.name, path.display())?,
            None => writeln!(writer, "{}: missing", tool.name)?,
        }
    }
    Ok(())
}

async fn prompt_for_installation<R: AsyncBufRead + Unpin, W: Write>(
    reader: &mut R,
    writer: &mut W,
) -> Result<bool> {
    loop {
        write!(writer, "Proceed with installation? [Y/n]: ")?;
        writer.flush()?;

        let mut input = String::new();
        if reader.read_line(&mut input).await? == 0 {
            return Ok(true);
        }

        match input.trim().to_ascii_lowercase().as_str() {
            "" | "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => writeln!(writer, "Please answer with y or n.")?,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use super::*;
    use tokio::io::AsyncRead;

    #[tokio::test]
    async fn prompts_default_to_yes_on_empty_input() {
        let mut input = io::Cursor::new("\n");
        let mut output = Vec::new();

        assert!(
            prompt_for_installation(&mut input, &mut output)
                .await
                .unwrap()
        );
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Proceed with installation? [Y/n]: "
        );
    }

    #[tokio::test]
    async fn prompts_accept_no_after_retry() {
        let mut input = io::Cursor::new("maybe\nn\n");
        let mut output = Vec::new();

        assert!(
            !prompt_for_installation(&mut input, &mut output)
                .await
                .unwrap()
        );
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Proceed with installation? [Y/n]: Please answer with y or n.\nProceed with installation? [Y/n]: "
        );
    }

    /// A reader that never produces input, like a terminal waiting on the user.
    struct PendingInput;

    impl AsyncRead for PendingInput {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncBufRead for PendingInput {
        fn poll_fill_buf(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
            Poll::Pending
        }

        fn consume(self: Pin<&mut Self>, _amt: usize) {}
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn pending_confirmation_does_not_starve_runtime_workers() {
        let prompt = tokio::spawn(async {
            let mut input = PendingInput;
            let mut output = Vec::new();
            prompt_for_installation(&mut input, &mut output).await
        });

        let timer = tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(20)).await;
        });

        // A blocking stdin read on the only worker would starve the timer task.
        tokio::time::timeout(Duration::from_secs(1), timer)
            .await
            .expect("prompt must not block the async worker")
            .expect("timer task must complete");

        prompt.abort();
    }
}
