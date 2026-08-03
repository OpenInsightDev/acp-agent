//! Local ACP agent process execution.

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};

use agent_client_protocol::AcpAgentConfig;
use anyhow::{Context, Result, bail};
use tokio::process::Command;

use crate::installer::binary::cache_binary_target;
use crate::installer::environment::program_available;
use crate::registry::{BinaryTarget, Environment, Platform, RegistryAgent, fetch_registry};

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandSpec {
    program: PathBuf,
    args: Vec<String>,
    env: Environment,
    current_dir: Option<PathBuf>,
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
) -> Result<AcpAgentConfig> {
    resolve_agent(agent_id, user_args).await?.into_acp_config()
}

async fn resolve_agent(agent_id: &str, user_args: &[String]) -> Result<CommandSpec> {
    let registry = fetch_registry().await?;
    let agent = registry
        .get_agent(agent_id)
        .with_context(|| format!("failed to resolve agent \"{agent_id}\" from registry"))?;
    resolve_agent_command(agent, user_args).await
}

impl CommandSpec {
    fn into_acp_config(self) -> Result<AcpAgentConfig> {
        let (command, args) = match self.current_dir {
            Some(current_dir) => {
                let mut wrapper_args = vec![
                    "__run-in-dir".to_string(),
                    path_argument(&current_dir, "working directory")?,
                    path_argument(&self.program, "executable path")?,
                ];
                wrapper_args.extend(self.args);
                let current_exe = std::env::current_exe()
                    .context("failed to locate acp-agent executable for binary agent wrapper")?;
                (current_exe, wrapper_args)
            }
            None => (self.program, self.args),
        };

        Ok(AcpAgentConfig::new(command).args(args).envs(self.env))
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
            return Ok(binary_command_spec(
                cached.executable_path,
                cached.extracted_dir,
                target,
                user_args,
            ));
        }
    }

    if let Some(npx) = &agent.distribution.npx {
        return Ok(npm_command_spec(
            program_available("npm")?,
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
    npm_available: bool,
    package: &str,
    default_args: Option<&[String]>,
    env: Option<&Environment>,
    user_args: &[String],
) -> CommandSpec {
    if npm_available {
        package_command_spec(
            "npm",
            &["exec", "--"],
            package,
            default_args,
            env,
            user_args,
        )
    } else {
        package_command_spec(
            "deno",
            &["x", "--allow-all", "--minimum-dependency-age", "0"],
            package,
            default_args,
            env,
            user_args,
        )
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
    args.extend(default_args.into_iter().flatten().cloned());
    args.extend_from_slice(user_args);

    CommandSpec {
        program: PathBuf::from(program),
        args,
        env: env.cloned().unwrap_or_default(),
        current_dir: None,
    }
}

fn binary_command_spec(
    executable_path: PathBuf,
    extracted_dir: PathBuf,
    target: &BinaryTarget,
    user_args: &[String],
) -> CommandSpec {
    let mut args: Vec<String> = target
        .args
        .as_deref()
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    args.extend_from_slice(user_args);

    CommandSpec {
        program: executable_path,
        args,
        env: target.env.clone().unwrap_or_default(),
        current_dir: Some(extracted_dir),
    }
}

async fn run_command(spec: CommandSpec, agent_id: &str) -> Result<ExitStatus> {
    let program = spec.program.display().to_string();
    spec.command()
        .status()
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
    };
    spec.command().status().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{AgentDistribution, NpxDistribution};

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

    #[test]
    fn resolves_npm_distribution_through_npm_when_available() {
        let agent = sample_npx_agent();
        let npx = agent.distribution.npx.as_ref().unwrap();
        let spec = npm_command_spec(
            true,
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
            false,
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
    fn converts_resolved_command_to_acp_process_config() {
        let spec = CommandSpec {
            program: PathBuf::from("agent-program"),
            args: strings(&["--stdio", "--model", "gpt-5"]),
            env: Environment::from([("AGENT_MODE".to_string(), "serve".to_string())]),
            current_dir: None,
        };

        let config = spec.into_acp_config().unwrap();

        assert_eq!(config.command(), std::path::Path::new("agent-program"));
        assert_eq!(config.arguments(), ["--stdio", "--model", "gpt-5"]);
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
        };

        let config = spec.into_acp_config().unwrap();

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
}
