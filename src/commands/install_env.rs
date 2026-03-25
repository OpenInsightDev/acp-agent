use std::env;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use anyhow::{Context, Result, anyhow, bail};
use tokio::process::Command;

const JS_TOOLS: [&str; 4] = ["npm", "yarn", "pnpm", "bun"];
const PYTHON_TOOLS: [&str; 2] = ["pip", "uv"];

/// Detects missing runtime dependencies and installs them if confirmed.
///
/// The command currently supports Bun (JS toolchain) and UV (Python runtime)
/// detection. It prints a plan, optionally prompts unless `assume_yes` is `true`,
/// runs the installers, and reports the verified installations.
pub async fn install_env<W: Write>(writer: &mut W, assume_yes: bool) -> Result<()> {
    let report = detect_environment()?;
    write_detection_report(&report, writer)?;

    let plan = InstallationPlan::from_report(&report);
    if plan.targets.is_empty() {
        writeln!(writer)?;
        writeln!(
            writer,
            "Environment already satisfies the requirements. No installation is needed."
        )?;
        return Ok(());
    }

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

    let confirmed = if assume_yes {
        true
    } else {
        let stdin = io::stdin();
        let mut reader = stdin.lock();
        prompt_for_installation(&mut reader, writer)?
    };

    if !confirmed {
        writeln!(writer, "Installation cancelled.")?;
        return Ok(());
    }

    writeln!(writer)?;
    writeln!(writer, "Starting installation...")?;
    let results = run_installation_plan(&plan).await?;

    writeln!(writer)?;
    for result in &results {
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

#[derive(Debug, Clone)]
struct EnvironmentReport {
    js: Vec<ToolAvailability>,
    python: Vec<ToolAvailability>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolAvailability {
    name: &'static str,
    path: Option<PathBuf>,
}

impl ToolAvailability {
    fn is_available(&self) -> bool {
        self.path.is_some()
    }
}

#[derive(Debug, Clone)]
struct InstallationPlan {
    targets: Vec<InstallTarget>,
}

impl InstallationPlan {
    fn from_report(report: &EnvironmentReport) -> Self {
        let mut targets = Vec::new();

        if report.js.iter().all(|tool| !tool.is_available()) {
            targets.push(InstallTarget::Bun);
        }

        if report.python.iter().all(|tool| !tool.is_available()) {
            targets.push(InstallTarget::Uv);
        }

        Self { targets }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallTarget {
    Bun,
    Uv,
}

impl InstallTarget {
    fn label(self) -> &'static str {
        match self {
            Self::Bun => "bun",
            Self::Uv => "uv",
        }
    }

    fn program(self) -> &'static str {
        self.label()
    }

    fn official_command_display(self) -> &'static str {
        if cfg!(windows) {
            match self {
                Self::Bun => r#"powershell -c "irm bun.sh/install.ps1|iex""#,
                Self::Uv => {
                    r#"powershell -ExecutionPolicy ByPass -c "irm https://astral.sh/uv/install.ps1 | iex""#
                }
            }
        } else {
            match self {
                Self::Bun => "curl -fsSL https://bun.com/install | bash",
                Self::Uv => "curl -LsSf https://astral.sh/uv/install.sh | sh",
            }
        }
    }

    fn shell_command(self) -> (&'static str, Vec<&'static str>) {
        if cfg!(windows) {
            match self {
                Self::Bun => ("powershell", vec!["-c", r#"irm bun.sh/install.ps1|iex"#]),
                Self::Uv => (
                    "powershell",
                    vec![
                        "-ExecutionPolicy",
                        "ByPass",
                        "-c",
                        r#"irm https://astral.sh/uv/install.ps1 | iex"#,
                    ],
                ),
            }
        } else {
            match self {
                Self::Bun => (
                    "bash",
                    vec!["-lc", "curl -fsSL https://bun.com/install | bash"],
                ),
                Self::Uv => (
                    "sh",
                    vec!["-c", "curl -LsSf https://astral.sh/uv/install.sh | sh"],
                ),
            }
        }
    }

    fn known_bin_directories(self, home: &Path) -> Vec<PathBuf> {
        match self {
            Self::Bun => vec![home.join(".bun").join("bin")],
            Self::Uv => {
                let mut directories = vec![home.join(".local").join("bin")];
                if cfg!(windows) {
                    directories.push(home.join(".cargo").join("bin"));
                }
                directories
            }
        }
    }

    fn requires_curl(self) -> bool {
        !cfg!(windows)
    }
}

#[derive(Debug, Clone)]
struct InstallationResult {
    target: InstallTarget,
    path: PathBuf,
    on_path: bool,
}

fn detect_environment() -> Result<EnvironmentReport> {
    Ok(EnvironmentReport {
        js: detect_tools(&JS_TOOLS)?,
        python: detect_tools(&PYTHON_TOOLS)?,
    })
}

fn detect_tools(programs: &[&'static str]) -> Result<Vec<ToolAvailability>> {
    programs
        .iter()
        .copied()
        .map(|name| {
            Ok(ToolAvailability {
                name,
                path: resolve_program(name)?,
            })
        })
        .collect()
}

fn write_detection_report<W: Write>(report: &EnvironmentReport, writer: &mut W) -> Result<()> {
    writeln!(writer, "Environment detection results:")?;
    writeln!(writer, "JavaScript tools:")?;
    write_tool_group(&report.js, writer)?;
    writeln!(writer, "Python tools:")?;
    write_tool_group(&report.python, writer)?;
    Ok(())
}

fn write_tool_group<W: Write>(tools: &[ToolAvailability], writer: &mut W) -> Result<()> {
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
        let bytes_read = reader.read_line(&mut input)?;
        if bytes_read == 0 {
            return Ok(true);
        }

        match input.trim().to_ascii_lowercase().as_str() {
            "" | "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => {
                writeln!(writer, "Please answer with y or n.")?;
            }
        }
    }
}

async fn run_installation_plan(plan: &InstallationPlan) -> Result<Vec<InstallationResult>> {
    match plan.targets.as_slice() {
        [] => Ok(Vec::new()),
        [target] => Ok(vec![install_and_verify(*target).await?]),
        [first, second] => {
            let (first_result, second_result) =
                tokio::join!(install_and_verify(*first), install_and_verify(*second));
            Ok(vec![first_result?, second_result?])
        }
        _ => unreachable!("install-env only supports bun and uv"),
    }
}

async fn install_and_verify(target: InstallTarget) -> Result<InstallationResult> {
    run_installer(target).await?;
    verify_installation(target).await
}

async fn run_installer(target: InstallTarget) -> Result<()> {
    ensure_installer_prerequisites(target)?;
    let (program, args) = target.shell_command();
    let output = Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("failed to run installer for {}", target.label()))?;

    if output.status.success() {
        return Ok(());
    }

    bail!(
        "installer for {} failed with status {}: {}",
        target.label(),
        display_status(output.status),
        render_output(&output.stdout, &output.stderr)
    )
}

fn ensure_installer_prerequisites(target: InstallTarget) -> Result<()> {
    if target.requires_curl() && resolve_program("curl")?.is_none() {
        return Err(anyhow!(
            "Cannot install {} because curl is not available in the current environment",
            target.label()
        ));
    }

    Ok(())
}

async fn verify_installation(target: InstallTarget) -> Result<InstallationResult> {
    let on_path = resolve_program(target.program())?.is_some();
    let home = dirs::home_dir().ok_or_else(|| anyhow!("unable to determine the home directory"))?;
    let path = if let Some(path) = resolve_program(target.program())? {
        path
    } else {
        resolve_program_with_directories(target.program(), &target.known_bin_directories(&home))?
            .ok_or_else(|| {
                anyhow!(
                    "verification failed for {}: {} was not found after installation",
                    target.label(),
                    target.program()
                )
            })?
    };

    verify_program_version(&path, target.label()).await?;
    Ok(InstallationResult {
        target,
        path,
        on_path,
    })
}

async fn verify_program_version(path: &Path, subject: &str) -> Result<()> {
    let output = Command::new(path)
        .arg("--version")
        .output()
        .await
        .with_context(|| format!("failed to run installer for {subject}"))?;

    if output.status.success() {
        return Ok(());
    }

    bail!(
        "verification failed for {subject}: {}",
        render_output(&output.stdout, &output.stderr)
    )
}

fn render_output(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(stderr).trim().to_string();

    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => "installer exited without any output".to_string(),
        (false, true) => stdout,
        (true, false) => stderr,
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

fn resolve_program(program: &str) -> Result<Option<PathBuf>> {
    resolve_program_with_directories(program, &Vec::new())
}

fn resolve_program_with_directories(
    program: &str,
    preferred_directories: &[PathBuf],
) -> Result<Option<PathBuf>> {
    let mut directories = preferred_directories.to_vec();
    if let Some(path_value) = env::var_os("PATH") {
        directories.extend(env::split_paths(&path_value));
    }

    for directory in directories {
        for extension in executable_extensions() {
            let candidate = directory.join(format!("{program}{extension}"));
            if candidate.is_file() {
                return Ok(Some(candidate));
            }
        }
    }

    Ok(None)
}

#[cfg(windows)]
fn executable_extensions() -> &'static [&'static str] {
    &["", ".exe", ".cmd", ".bat", ".ps1"]
}

#[cfg(not(windows))]
fn executable_extensions() -> &'static [&'static str] {
    &[""]
}

fn display_status(status: ExitStatus) -> String {
    if let Some(code) = status.code() {
        return code.to_string();
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("signal {signal}");
        }
    }

    "unknown".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(js_available: &[&str], python_available: &[&str]) -> EnvironmentReport {
        EnvironmentReport {
            js: JS_TOOLS
                .iter()
                .map(|name| ToolAvailability {
                    name,
                    path: js_available
                        .contains(name)
                        .then(|| PathBuf::from(format!("/bin/{name}"))),
                })
                .collect(),
            python: PYTHON_TOOLS
                .iter()
                .map(|name| ToolAvailability {
                    name,
                    path: python_available
                        .contains(name)
                        .then(|| PathBuf::from(format!("/bin/{name}"))),
                })
                .collect(),
        }
    }

    #[test]
    fn plans_bun_and_uv_when_both_toolchains_are_missing() {
        let plan = InstallationPlan::from_report(&report(&[], &[]));

        assert_eq!(plan.targets, vec![InstallTarget::Bun, InstallTarget::Uv]);
    }

    #[test]
    fn skips_installation_when_a_js_and_python_tool_are_available() {
        let plan = InstallationPlan::from_report(&report(&["pnpm"], &["pip"]));

        assert!(plan.targets.is_empty());
    }

    #[test]
    fn prompts_default_to_yes_on_empty_input() {
        let mut input = io::Cursor::new("\n");
        let mut output = Vec::new();

        let confirmed = prompt_for_installation(&mut input, &mut output).unwrap();

        assert!(confirmed);
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Proceed with installation? [Y/n]: "
        );
    }

    #[test]
    fn prompts_accept_no_after_retry() {
        let mut input = io::Cursor::new("maybe\nn\n");
        let mut output = Vec::new();

        let confirmed = prompt_for_installation(&mut input, &mut output).unwrap();

        assert!(!confirmed);
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Proceed with installation? [Y/n]: Please answer with y or n.\nProceed with installation? [Y/n]: "
        );
    }

    #[test]
    fn reports_clear_error_when_curl_is_missing() {
        let error =
            anyhow!("Cannot install bun because curl is not available in the current environment");

        assert_eq!(
            error.to_string(),
            "Cannot install bun because curl is not available in the current environment"
        );
    }
}
