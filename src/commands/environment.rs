use std::io::{self, BufRead, Write};

use anyhow::Result;

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

pub(super) fn confirm_installation<W: Write>(writer: &mut W) -> Result<bool> {
    let stdin = io::stdin();
    prompt_for_installation(&mut stdin.lock(), writer)
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

fn prompt_for_installation<R: BufRead, W: Write>(reader: &mut R, writer: &mut W) -> Result<bool> {
    loop {
        write!(writer, "Proceed with installation? [Y/n]: ")?;
        writer.flush()?;

        let mut input = String::new();
        if reader.read_line(&mut input)? == 0 {
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
    use super::*;

    #[test]
    fn prompts_default_to_yes_on_empty_input() {
        let mut input = io::Cursor::new("\n");
        let mut output = Vec::new();

        assert!(prompt_for_installation(&mut input, &mut output).unwrap());
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Proceed with installation? [Y/n]: "
        );
    }

    #[test]
    fn prompts_accept_no_after_retry() {
        let mut input = io::Cursor::new("maybe\nn\n");
        let mut output = Vec::new();

        assert!(!prompt_for_installation(&mut input, &mut output).unwrap());
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Proceed with installation? [Y/n]: Please answer with y or n.\nProceed with installation? [Y/n]: "
        );
    }
}
