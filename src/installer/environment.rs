use std::env;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use anyhow::{Context, Result, anyhow, bail};
use tokio::process::Command;

use crate::process;

const JS_TOOLS: [&str; 2] = ["npm", "deno"];
const PYTHON_TOOLS: [&str; 1] = ["uv"];

/// Availability of the supported local toolchains.
#[derive(Debug, Clone)]
pub struct EnvironmentReport {
    /// JavaScript package runners used for npm distributions.
    pub js: Vec<ToolAvailability>,
    /// Python package runners used for uvx distributions.
    pub python: Vec<ToolAvailability>,
}

/// Availability of one executable on the current machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolAvailability {
    /// Executable name.
    pub name: &'static str,
    /// Resolved executable path when available.
    pub path: Option<PathBuf>,
}

impl ToolAvailability {
    /// Whether the executable was found on `PATH`.
    pub fn is_available(&self) -> bool {
        self.path.is_some()
    }
}

/// Toolchains that should be installed to support agent distributions.
#[derive(Debug, Clone)]
pub struct InstallationPlan {
    /// Ordered installation targets.
    pub targets: Vec<InstallTarget>,
}

/// Creates the installation plan for an environment report.
pub fn plan_installation(report: &EnvironmentReport) -> InstallationPlan {
    let mut targets = Vec::new();

    if report.js.iter().all(|tool| !tool.is_available()) {
        targets.push(InstallTarget::Deno);
    }

    if report.python.iter().all(|tool| !tool.is_available()) {
        targets.push(InstallTarget::Uv);
    }

    InstallationPlan { targets }
}

/// One installer command as a single structured specification.
///
/// The confirmation display and the executed `Command` are both derived from
/// this specification, so the two can never drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InstallerCommand {
    /// Program that executes the installer script.
    program: &'static str,
    /// Fixed arguments, with the installer script as the last element.
    args: &'static [&'static str],
}

impl InstallerCommand {
    /// Human-readable command line shown before execution.
    fn display(self) -> String {
        let (script, prefix) = self
            .args
            .split_last()
            .expect("every installer command has a script");
        let mut rendered = self.program.to_string();
        for arg in prefix {
            rendered.push(' ');
            rendered.push_str(arg);
        }
        format!("{rendered} \"{}\"", *script)
    }
}

/// A supported local toolchain installer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallTarget {
    /// Deno supplies an npm-compatible runner when npm is absent.
    Deno,
    /// uv supplies the uvx runner for Python distributions.
    Uv,
}

impl InstallTarget {
    /// Stable executable label for the target.
    pub fn label(self) -> &'static str {
        match self {
            Self::Deno => "deno",
            Self::Uv => "uv",
        }
    }

    /// Executable expected after installation.
    pub fn program(self) -> &'static str {
        self.label()
    }

    /// Official installer command shown before execution.
    pub fn official_command_display(self) -> String {
        self.installer_command().display()
    }

    /// Structured installer command for this target.
    fn installer_command(self) -> InstallerCommand {
        if cfg!(windows) {
            match self {
                Self::Deno => InstallerCommand {
                    program: "powershell",
                    args: &["-c", r#"irm https://deno.land/install.ps1 | iex"#],
                },
                Self::Uv => InstallerCommand {
                    program: "powershell",
                    args: &[
                        "-ExecutionPolicy",
                        "ByPass",
                        "-c",
                        r#"irm https://astral.sh/uv/install.ps1 | iex"#,
                    ],
                },
            }
        } else {
            match self {
                Self::Deno => InstallerCommand {
                    program: "sh",
                    args: &["-c", "curl -fsSL https://deno.land/install.sh | sh"],
                },
                Self::Uv => InstallerCommand {
                    program: "sh",
                    args: &["-c", "curl -LsSf https://astral.sh/uv/install.sh | sh"],
                },
            }
        }
    }

    fn known_bin_directories(self, home: &Path) -> Vec<PathBuf> {
        match self {
            Self::Deno => vec![home.join(".deno").join("bin")],
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

/// Verified result of installing one local toolchain.
#[derive(Debug, Clone)]
pub struct InstallationResult {
    /// Installed toolchain.
    pub target: InstallTarget,
    /// Verified executable path.
    pub path: PathBuf,
    /// Whether the executable is already on the current process's `PATH`.
    pub on_path: bool,
}

/// Detects the supported toolchains available in the current environment.
pub fn detect_environment() -> Result<EnvironmentReport> {
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

/// Runs and verifies every target in an installation plan.
///
/// Targets run sequentially in plan order so the installers never modify user
/// directories or shell profiles concurrently, and the first failure aborts
/// the remaining targets with an explicit error.
pub async fn install_plan(plan: &InstallationPlan) -> Result<Vec<InstallationResult>> {
    install_plan_with(plan, install_and_verify).await
}

async fn install_plan_with<F, Fut>(
    plan: &InstallationPlan,
    install: F,
) -> Result<Vec<InstallationResult>>
where
    F: Fn(InstallTarget) -> Fut,
    Fut: Future<Output = Result<InstallationResult>>,
{
    let mut results = Vec::with_capacity(plan.targets.len());
    for target in &plan.targets {
        results.push(install(*target).await?);
    }
    Ok(results)
}

async fn install_and_verify(target: InstallTarget) -> Result<InstallationResult> {
    run_installer(target).await?;
    verify_installation(target).await
}

async fn run_installer(target: InstallTarget) -> Result<()> {
    ensure_installer_prerequisites(target)?;
    let command = target.installer_command();
    let mut process = Command::new(command.program);
    process.args(command.args);
    let output = process::output(&mut process)
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
    let home = dirs::home_dir().ok_or_else(|| anyhow!("unable to determine the home directory"))?;
    let on_path = resolve_program(target.program())?;
    let on_path_available = on_path.is_some();
    let path = match on_path {
        Some(path) => path,
        None => resolve_program_with_directories(
            target.program(),
            &target.known_bin_directories(&home),
        )?
        .ok_or_else(|| {
            anyhow!(
                "verification failed for {}: {} was not found after installation",
                target.label(),
                target.program()
            )
        })?,
    };

    verify_program_version(&path, target.label()).await?;
    Ok(InstallationResult {
        target,
        path,
        on_path: on_path_available,
    })
}

async fn verify_program_version(path: &Path, subject: &str) -> Result<()> {
    let mut process = Command::new(path);
    process.arg("--version");
    let output = process::output(&mut process)
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

pub(crate) fn program_available(program: &str) -> Result<bool> {
    Ok(resolve_program(program)?.is_some())
}

/// Resolves `program` against the preferred directories followed by `PATH`.
///
/// Resolution delegates to `which`, so a candidate must be an executable
/// regular file on Unix and Windows resolution follows the active `PATHEXT`
/// instead of a hard-coded extension list.
fn resolve_program_with_directories(
    program: &str,
    preferred_directories: &[PathBuf],
) -> Result<Option<PathBuf>> {
    let mut directories = preferred_directories.to_vec();
    if let Some(path_value) = env::var_os("PATH") {
        directories.extend(env::split_paths(&path_value));
    }
    if directories.is_empty() {
        return Ok(None);
    }
    let path_list =
        env::join_paths(directories).context("failed to join tool search directories")?;
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match which::which_in(program, Some(path_list), cwd) {
        Ok(path) => Ok(Some(path)),
        Err(_) => Ok(None),
    }
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
    use crate::installer::test_support::ENV_LOCK;

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
    fn plans_deno_and_uv_when_both_toolchains_are_missing() {
        let plan = plan_installation(&report(&[], &[]));

        assert_eq!(plan.targets, vec![InstallTarget::Deno, InstallTarget::Uv]);
    }

    #[test]
    fn skips_deno_installation_when_npm_is_available() {
        let plan = plan_installation(&report(&["npm"], &["uv"]));

        assert!(plan.targets.is_empty());
    }

    #[test]
    fn skips_deno_installation_when_deno_is_available() {
        let plan = plan_installation(&report(&["deno"], &["uv"]));

        assert!(plan.targets.is_empty());
    }

    #[test]
    fn installer_command_display_is_derived_from_the_same_specification() {
        for target in [InstallTarget::Deno, InstallTarget::Uv] {
            let command = target.installer_command();
            let display = command.display();
            let (script, prefix) = command
                .args
                .split_last()
                .expect("every installer command has a script");

            assert!(display.starts_with(command.program), "{display}");
            for arg in prefix {
                assert!(display.contains(arg), "{display} missing {arg}");
            }
            assert!(
                display.ends_with(&format!("\"{}\"", *script)),
                "{display} must quote the script"
            );
        }
    }

    #[tokio::test]
    async fn installs_targets_sequentially_in_plan_order() {
        use std::sync::{Arc, Mutex};

        let executed = Arc::new(Mutex::new(Vec::new()));
        let plan = InstallationPlan {
            targets: vec![InstallTarget::Deno, InstallTarget::Uv],
        };

        let results = install_plan_with(&plan, |target| {
            let executed = Arc::clone(&executed);
            async move {
                executed.lock().unwrap().push(target);
                Ok(InstallationResult {
                    target,
                    path: PathBuf::from(format!("/tmp/{}", target.label())),
                    on_path: false,
                })
            }
        })
        .await
        .unwrap();

        assert_eq!(
            *executed.lock().unwrap(),
            vec![InstallTarget::Deno, InstallTarget::Uv]
        );
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn installs_targets_stop_at_the_first_failure() {
        use std::sync::{Arc, Mutex};

        let executed = Arc::new(Mutex::new(Vec::new()));
        let plan = InstallationPlan {
            targets: vec![InstallTarget::Deno, InstallTarget::Uv],
        };

        let error = install_plan_with(&plan, |target| {
            let executed = Arc::clone(&executed);
            async move {
                executed.lock().unwrap().push(target);
                if target == InstallTarget::Deno {
                    Err(anyhow!("deno installer failed"))
                } else {
                    Ok(InstallationResult {
                        target,
                        path: PathBuf::from("/tmp/uv"),
                        on_path: false,
                    })
                }
            }
        })
        .await
        .unwrap_err();

        assert_eq!(error.to_string(), "deno installer failed");
        assert_eq!(
            *executed.lock().unwrap(),
            vec![InstallTarget::Deno],
            "later targets must not run after the first installer fails"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    // The guard intentionally spans the await below: it serializes tests that
    // mutate the process-wide `PATH`, which the test runtime would otherwise
    // run in parallel. No other task can hold it concurrently, so no deadlock.
    #[allow(clippy::await_holding_lock)]
    async fn verify_installation_derives_path_and_on_path_from_one_resolution() {
        use std::os::unix::fs::PermissionsExt;
        use tempfile::tempdir;

        let _env_guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp_dir = tempdir().unwrap();
        let bin = temp_dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let deno = bin.join("deno");
        std::fs::write(&deno, "#!/bin/sh\necho deno 2.0.0\n").unwrap();
        std::fs::set_permissions(&deno, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Prepend the fake bin dir to PATH so the single resolution finds deno
        // both on PATH and in the preferred directories; the test restores the
        // previous value before returning.
        let previous_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var(
                "PATH",
                format!(
                    "{}:{}",
                    bin.display(),
                    previous_path.clone().unwrap_or_default().to_string_lossy()
                ),
            );
        }
        let result = verify_installation(InstallTarget::Deno).await;
        match previous_path {
            Some(previous) => unsafe { std::env::set_var("PATH", previous) },
            None => unsafe { std::env::remove_var("PATH") },
        }

        let result = result.unwrap();
        assert_eq!(result.path, deno);
        assert!(result.on_path);
    }

    #[cfg(unix)]
    #[test]
    fn unix_resolution_rejects_non_executable_regular_files() {
        use std::os::unix::fs::PermissionsExt;
        use tempfile::tempdir;

        let _env_guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp_dir = tempdir().unwrap();
        let bin = temp_dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let tool = bin.join("tool");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o644)).unwrap();

        let previous_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", &bin);
        }
        let without_exec_bit = resolve_program("tool");
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let with_exec_bit = resolve_program("tool");
        match previous_path {
            Some(previous) => unsafe { std::env::set_var("PATH", previous) },
            None => unsafe { std::env::remove_var("PATH") },
        }

        assert!(
            without_exec_bit.unwrap().is_none(),
            "a non-executable regular file must not be treated as available"
        );
        assert_eq!(with_exec_bit.unwrap().unwrap(), tool);
    }

    #[cfg(unix)]
    #[test]
    fn duplicate_path_entries_resolve_to_the_first_executable_match() {
        use std::os::unix::fs::PermissionsExt;
        use tempfile::tempdir;

        let _env_guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp_dir = tempdir().unwrap();
        let first = temp_dir.path().join("first");
        let second = temp_dir.path().join("second");
        for dir in [&first, &second] {
            std::fs::create_dir_all(dir).unwrap();
        }
        for (dir, content) in [(&first, "first"), (&second, "second")] {
            let tool = dir.join("tool");
            std::fs::write(&tool, content).unwrap();
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let previous_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", format!("{}:{}", first.display(), second.display()));
        }
        let resolved = resolve_program("tool").unwrap().unwrap();
        match previous_path {
            Some(previous) => unsafe { std::env::set_var("PATH", previous) },
            None => unsafe { std::env::remove_var("PATH") },
        }

        assert_eq!(resolved, first.join("tool"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_resolution_finds_executable_extension() {
        use tempfile::tempdir;

        let _env_guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp_dir = tempdir().unwrap();
        let bin = temp_dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("tool.exe"), b"tool").unwrap();

        // The real `PATHEXT` is left untouched so `which` initializes its
        // per-process extension cache from the actual value; `.EXE` is always
        // present on Windows. A hard-coded extension list would also pass this
        // test, so it guards the delegation itself rather than PATHEXT parsing,
        // which `which` owns.
        let previous_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", &bin);
        }
        let resolved = resolve_program("tool");
        match previous_path {
            Some(previous) => unsafe { std::env::set_var("PATH", previous) },
            None => unsafe { std::env::remove_var("PATH") },
        }

        assert_eq!(resolved.unwrap().unwrap(), bin.join("tool.exe"));
    }
}
