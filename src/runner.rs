//! Local ACP agent process execution.

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;

use agent_client_protocol::AcpAgentConfig;
use anyhow::{Context, Result, bail};
use tokio::process::Command;

use crate::installer::binary::{CachedBinary, cache_binary_target};
use crate::installer::cache::BinaryCacheLock;
use crate::installer::environment::program_available;
use crate::process;
use crate::registry::{BinaryTarget, Environment, Platform, RegistryAgent, fetch_registry};

#[derive(Debug, Clone)]
struct CommandSpec {
    program: PathBuf,
    args: Vec<String>,
    env: Environment,
    current_dir: Option<PathBuf>,
    cache_use_lease: Option<Arc<BinaryCacheLock>>,
}

pub(crate) struct ResolvedAgentConfig {
    pub(crate) config: AcpAgentConfig,
    pub(crate) cache_use_lease: Option<Arc<BinaryCacheLock>>,
}

/// Executable that runs a package-based distribution, decided once and shared
/// by the installer (which prepares the runner's cache) and the runner (which
/// executes the package) so the two commands can never choose different tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageRunner {
    /// `npm` executes npm packages through `npm exec`.
    Npm,
    /// Deno executes npm packages through `deno x` when npm is unavailable.
    Deno,
    /// uv executes Python packages through `uvx`.
    Uvx,
}

impl PackageRunner {
    /// Executable that implements the runner.
    pub fn program(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Deno => "deno",
            Self::Uvx => "uvx",
        }
    }
}

impl std::fmt::Display for PackageRunner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.program())
    }
}

/// Chooses the runner for an npm distribution: `npm` when it is available,
/// otherwise Deno's `deno x`. Both `install` and `run`/`serve` resolve through
/// this function, so the cache an install prepares always matches the command
/// that later executes the package.
pub(crate) fn npm_package_runner() -> Result<PackageRunner> {
    if program_available("npm")? {
        Ok(PackageRunner::Npm)
    } else {
        Ok(PackageRunner::Deno)
    }
}

/// Resolves the shared runner for an agent's package distribution, if any.
///
/// Binary distributions return `None`; the installer prepares those through
/// the binary cache instead.
pub(crate) fn package_runner_for(agent: &RegistryAgent) -> Result<Option<PackageRunner>> {
    if agent.distribution.npx.is_some() {
        return Ok(Some(npm_package_runner()?));
    }
    if agent.distribution.uvx.is_some() {
        return Ok(Some(PackageRunner::Uvx));
    }
    Ok(None)
}

/// Runs a registry agent locally with its standard streams attached to the terminal.
pub async fn run_agent(agent_id: &str, user_args: &[String]) -> Result<ExitStatus> {
    let spec = resolve_agent(agent_id, user_args).await?;
    run_command(spec, agent_id).await
}

/// Resolves a registry agent into the process configuration used by ACP transports.
pub(crate) async fn resolve_agent_config(
    agent_id: &str,
    user_args: &[String],
) -> Result<ResolvedAgentConfig> {
    let registry = fetch_registry().await?;
    let agent = registry
        .get_agent(agent_id)
        .with_context(|| format!("failed to resolve agent \"{agent_id}\" from registry"))?;
    resolve_agent_config_from_registry_agent(agent, user_args).await
}

/// Resolves an already-fetched registry agent into an ACP process configuration.
///
/// Callers that need to distinguish an unavailable registry from an unknown
/// agent can fetch once, select the agent, then use this resolver without a
/// second registry request.
pub(crate) async fn resolve_agent_config_from_registry_agent(
    agent: &RegistryAgent,
    user_args: &[String],
) -> Result<ResolvedAgentConfig> {
    resolve_agent_command(agent, user_args)
        .await?
        .into_resolved_config()
}

async fn resolve_agent(agent_id: &str, user_args: &[String]) -> Result<CommandSpec> {
    let registry = fetch_registry().await?;
    let agent = registry
        .get_agent(agent_id)
        .with_context(|| format!("failed to resolve agent \"{agent_id}\" from registry"))?;
    resolve_agent_command(agent, user_args).await
}

impl CommandSpec {
    fn into_resolved_config(self) -> Result<ResolvedAgentConfig> {
        // Every served agent goes through this small wrapper. On Unix the
        // wrapped command remains in the process group created by `AcpAgent`;
        // on Windows the wrapper owns a kill-on-close Job Object. Both paths
        // terminate package-runner descendants as well as binary agents when
        // the transport connection is cancelled.
        let current_dir = match self.current_dir {
            Some(current_dir) => current_dir,
            None => std::env::current_dir()
                .context("failed to resolve the agent process working directory")?,
        };
        let mut args = vec![
            "__run-in-dir".to_string(),
            path_argument(&current_dir, "working directory")?,
            path_argument(&self.program, "executable path")?,
        ];
        args.extend(self.args);
        let command = std::env::current_exe()
            .context("failed to locate acp-agent executable for process supervision")?;

        Ok(ResolvedAgentConfig {
            config: AcpAgentConfig::new(command).args(args).envs(self.env),
            cache_use_lease: self.cache_use_lease,
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args).envs(&self.env);
        if let Some(current_dir) = &self.current_dir {
            command.current_dir(current_dir);
        }
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        command
    }
}

fn path_argument(path: &Path, description: &str) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("agent {description} is not valid UTF-8: {path:?}"))
}

async fn resolve_agent_command(agent: &RegistryAgent, user_args: &[String]) -> Result<CommandSpec> {
    if let Some(binary) = &agent.distribution.binary {
        let platform = Platform::current()?;
        if let Some(target) = binary.for_platform(platform) {
            let cached = cache_binary_target(agent, platform, target).await?;
            return Ok(binary_command_spec(cached, target, user_args));
        }
    }

    if let Some(npx) = &agent.distribution.npx {
        // The runner choice is shared with `install` (see `npm_package_runner`),
        // so the cache an install prepares is the cache this command reads.
        return Ok(npm_command_spec(
            npm_package_runner()?,
            &npx.package,
            npx.args.as_deref(),
            npx.env.as_ref(),
            user_args,
        ));
    }

    if let Some(uvx) = &agent.distribution.uvx {
        return Ok(package_command_spec(
            "uvx",
            &[],
            &uvx.package,
            uvx.args.as_deref(),
            uvx.env.as_ref(),
            user_args,
        ));
    }

    bail!(
        "agent \"{}\" does not have a runnable distribution",
        agent.id
    )
}

fn npm_command_spec(
    runner: PackageRunner,
    package: &str,
    default_args: Option<&[String]>,
    env: Option<&Environment>,
    user_args: &[String],
) -> CommandSpec {
    match runner {
        PackageRunner::Npm => package_command_spec(
            "npm",
            &["exec", "--"],
            package,
            default_args,
            env,
            user_args,
        ),
        PackageRunner::Deno => package_command_spec(
            "deno",
            &["x", "--allow-all", "--minimum-dependency-age", "0"],
            package,
            default_args,
            env,
            user_args,
        ),
        // uvx belongs to uvx distributions; the shared runner never routes an
        // npm distribution here.
        PackageRunner::Uvx => unreachable!("npm distributions never use the uvx runner"),
    }
}

fn package_command_spec(
    program: &str,
    runner_args: &[&str],
    package: &str,
    default_args: Option<&[String]>,
    env: Option<&Environment>,
    user_args: &[String],
) -> CommandSpec {
    let mut args: Vec<String> = runner_args.iter().map(|arg| (*arg).to_string()).collect();
    args.push(package.to_string());
    if let Some(default_args) = default_args {
        args.extend_from_slice(default_args);
    }
    args.extend_from_slice(user_args);

    CommandSpec {
        program: PathBuf::from(program),
        args,
        env: env.cloned().unwrap_or_default(),
        current_dir: None,
        cache_use_lease: None,
    }
}

fn binary_command_spec(
    cached: CachedBinary,
    target: &BinaryTarget,
    user_args: &[String],
) -> CommandSpec {
    let mut args = target.args.clone().unwrap_or_default();
    args.extend_from_slice(user_args);

    CommandSpec {
        program: cached.executable_path,
        args,
        env: target.env.clone().unwrap_or_default(),
        current_dir: Some(cached.extracted_dir),
        cache_use_lease: cached.cache_use_lease,
    }
}

async fn run_command(spec: CommandSpec, agent_id: &str) -> Result<ExitStatus> {
    let program = spec.program.display().to_string();
    let mut command = spec.command();
    process::status(&mut command)
        .await
        .with_context(|| format!("failed to run {program} for {agent_id}"))
}

/// Runs a command with inherited stdio from a specific working directory.
pub(crate) async fn run_in_directory(
    current_dir: &Path,
    program: &Path,
    args: Vec<String>,
) -> std::io::Result<ExitStatus> {
    let spec = CommandSpec {
        program: program.to_owned(),
        args,
        env: Environment::new(),
        current_dir: Some(current_dir.to_owned()),
        cache_use_lease: None,
    };
    let mut command = spec.command();
    // This is invoked by the hidden ACP wrapper.  Keep the wrapped process in
    // the wrapper's supervisor group on Unix so cancellation of the wrapper
    // also terminates the real agent and its descendants.
    process::status_in_supervisor_group(&mut command).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{AgentDistribution, NpxDistribution, UvxDistribution};

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn sample_npx_agent() -> RegistryAgent {
        RegistryAgent {
            id: "demo".to_string(),
            name: "Demo".to_string(),
            version: "1.0.0".to_string(),
            description: String::new(),
            repository: None,
            website: None,
            authors: Vec::new(),
            license: "MIT".to_string(),
            icon: None,
            distribution: AgentDistribution {
                binary: None,
                npx: Some(NpxDistribution {
                    package: "@acme/demo".to_string(),
                    args: Some(vec!["--stdio".to_string()]),
                    env: Some(Environment::from([(
                        "DEMO_MODE".to_string(),
                        "local".to_string(),
                    )])),
                }),
                uvx: None,
            },
        }
    }

    fn sample_uvx_agent() -> RegistryAgent {
        RegistryAgent {
            distribution: AgentDistribution {
                binary: None,
                npx: None,
                uvx: Some(UvxDistribution {
                    package: "acme-demo".to_string(),
                    args: Some(vec!["--stdio".to_string()]),
                    env: Some(Environment::from([(
                        "DEMO_MODE".to_string(),
                        "local".to_string(),
                    )])),
                }),
            },
            ..sample_npx_agent()
        }
    }

    #[test]
    fn resolves_npm_distribution_through_npm_when_available() {
        let agent = sample_npx_agent();
        let npx = agent.distribution.npx.as_ref().unwrap();
        let spec = npm_command_spec(
            PackageRunner::Npm,
            &npx.package,
            npx.args.as_deref(),
            npx.env.as_ref(),
            &["--model".to_string(), "gpt-5".to_string()],
        );

        assert_eq!(spec.program, Path::new("npm"));
        assert_eq!(
            spec.args,
            strings(&["exec", "--", "@acme/demo", "--stdio", "--model", "gpt-5"])
        );
        assert_eq!(
            spec.env,
            Environment::from([("DEMO_MODE".to_string(), "local".to_string())])
        );
        assert_eq!(spec.current_dir, None);
    }

    #[test]
    fn resolves_npm_distribution_through_deno_when_npm_is_unavailable() {
        let agent = sample_npx_agent();
        let npx = agent.distribution.npx.as_ref().unwrap();
        let spec = npm_command_spec(
            PackageRunner::Deno,
            &npx.package,
            npx.args.as_deref(),
            npx.env.as_ref(),
            &["--model".to_string(), "gpt-5".to_string()],
        );

        assert_eq!(spec.program, Path::new("deno"));
        assert_eq!(
            spec.args,
            strings(&[
                "x",
                "--allow-all",
                "--minimum-dependency-age",
                "0",
                "@acme/demo",
                "--stdio",
                "--model",
                "gpt-5",
            ])
        );
    }

    #[test]
    fn package_runner_is_shared_between_install_and_run() {
        // `package_runner_for` drives the installer's cache preparation, and
        // the runner builds its command from the same decision. Binary agents
        // have no package runner; uvx agents always run through uvx.
        let binary_agent = RegistryAgent {
            distribution: AgentDistribution {
                binary: Some(crate::registry::BinaryDistribution::default()),
                npx: None,
                uvx: None,
            },
            ..sample_npx_agent()
        };
        assert_eq!(
            package_runner_for(&binary_agent).unwrap(),
            None,
            "binary distributions are prepared through the binary cache"
        );
        assert_eq!(
            package_runner_for(&sample_uvx_agent()).unwrap(),
            Some(PackageRunner::Uvx)
        );
        assert_eq!(PackageRunner::Uvx.program(), "uvx");
        assert_eq!(PackageRunner::Npm.to_string(), "npm");
        assert_eq!(PackageRunner::Deno.to_string(), "deno");
    }

    #[cfg(unix)]
    #[tokio::test]
    // The guard intentionally spans the await below: it serializes tests that
    // mutate the process-wide `PATH`, which the test runtime would otherwise
    // run in parallel. No other task can hold it concurrently, so no deadlock.
    #[allow(clippy::await_holding_lock)]
    async fn shared_runner_decision_follows_npm_availability() {
        use std::os::unix::fs::PermissionsExt;
        use tempfile::tempdir;

        use crate::installer::test_support::ENV_LOCK;

        let _env_guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp_dir = tempdir().unwrap();
        let npm_dir = temp_dir.path().join("with-npm");
        std::fs::create_dir_all(&npm_dir).unwrap();
        let fake_npm = npm_dir.join("npm");
        std::fs::write(&fake_npm, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&fake_npm, std::fs::Permissions::from_mode(0o755)).unwrap();
        let empty_dir = temp_dir.path().join("without-npm");
        std::fs::create_dir_all(&empty_dir).unwrap();

        let previous_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", &npm_dir);
        }
        let with_npm = npm_package_runner().unwrap();
        unsafe {
            std::env::set_var("PATH", &empty_dir);
        }
        let without_npm = npm_package_runner().unwrap();
        match previous_path {
            Some(previous) => unsafe { std::env::set_var("PATH", previous) },
            None => unsafe { std::env::remove_var("PATH") },
        }

        assert_eq!(with_npm, PackageRunner::Npm);
        assert_eq!(without_npm, PackageRunner::Deno);

        // The same decision drives the command the runner executes: with npm
        // available run uses `npm exec`, without npm it uses `deno x`, so an
        // install that prepared the corresponding cache is consumed by run.
        let npx = sample_npx_agent();
        let npx = npx.distribution.npx.unwrap();
        let spec = npm_command_spec(
            with_npm,
            &npx.package,
            npx.args.as_deref(),
            npx.env.as_ref(),
            &[],
        );
        assert_eq!(spec.program, Path::new("npm"));
        let spec = npm_command_spec(
            without_npm,
            &npx.package,
            npx.args.as_deref(),
            npx.env.as_ref(),
            &[],
        );
        assert_eq!(spec.program, Path::new("deno"));
    }

    #[test]
    fn wraps_package_process_config_for_process_tree_supervision() {
        let spec = CommandSpec {
            program: PathBuf::from("agent-program"),
            args: strings(&["--stdio", "--model", "gpt-5"]),
            env: Environment::from([("AGENT_MODE".to_string(), "serve".to_string())]),
            current_dir: None,
            cache_use_lease: None,
        };

        let config = spec.into_resolved_config().unwrap().config;

        assert_eq!(config.command(), std::env::current_exe().unwrap());
        let arguments = config.arguments();
        assert_eq!(arguments[0], "__run-in-dir");
        assert_eq!(arguments[1], std::env::current_dir().unwrap());
        assert_eq!(
            arguments[2..],
            ["agent-program", "--stdio", "--model", "gpt-5"]
        );
        assert_eq!(
            config.environment().get("AGENT_MODE"),
            Some(&"serve".to_string())
        );
    }

    #[test]
    fn wraps_binary_process_config_to_preserve_working_directory() {
        let spec = CommandSpec {
            program: PathBuf::from("/cache/demo/bin/agent"),
            args: strings(&["--stdio"]),
            env: Environment::from([("AGENT_MODE".to_string(), "serve".to_string())]),
            current_dir: Some(PathBuf::from("/cache/demo")),
            cache_use_lease: None,
        };

        let config = spec.into_resolved_config().unwrap().config;

        assert_eq!(config.command(), std::env::current_exe().unwrap());
        assert_eq!(
            config.arguments(),
            [
                "__run-in-dir",
                "/cache/demo",
                "/cache/demo/bin/agent",
                "--stdio",
            ]
        );
        assert_eq!(
            config.environment().get("AGENT_MODE"),
            Some(&"serve".to_string())
        );
    }

    #[tokio::test]
    async fn resolves_config_from_supplied_agent_without_fetching_registry() {
        let config = resolve_agent_config_from_registry_agent(
            &sample_uvx_agent(),
            &["--model".to_string(), "gpt-5".to_string()],
        )
        .await
        .unwrap();

        assert_eq!(config.config.command(), std::env::current_exe().unwrap());
        let arguments = config.config.arguments();
        assert_eq!(arguments[0], "__run-in-dir");
        assert_eq!(
            arguments[2..],
            ["uvx", "acme-demo", "--stdio", "--model", "gpt-5"]
        );
        assert_eq!(
            config.config.environment().get("DEMO_MODE"),
            Some(&"local".to_string())
        );
    }
}
