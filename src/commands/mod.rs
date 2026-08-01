use std::io::Write;
use std::path::PathBuf;
use std::process::ExitStatus;

use anyhow::Context;
use clap::{Parser, Subcommand};

/// Agent installation command.
pub mod install;
/// Registry listing output helpers.
pub mod list;
/// Local agent execution command.
pub mod run;
/// Registry search output helpers.
pub mod search;
/// ACP HTTP agent serving command.
pub mod serve;

/// CLI arguments consumed by the `acp-agent` binary.
#[derive(Debug, Parser)]
#[command(
    name = "acp-agent",
    version,
    about = "Discover, install, and run ACP agents locally."
)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Internal process wrapper used to preserve binary distribution working directories.
    #[command(name = "__run-in-dir", hide = true, trailing_var_arg = true)]
    RunInDir {
        current_dir: PathBuf,
        program: PathBuf,
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// List every published agent.
    List,
    /// Install an agent from its preferred registry distribution.
    Install { agent_id: String },
    /// Install Deno or uv when no compatible local toolchain exists.
    InstallEnv {
        /// Skip the confirmation prompt.
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
    /// Run an agent locally over stdio.
    #[command(trailing_var_arg = true)]
    Run {
        agent_id: String,
        /// Arguments passed to the agent process.
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Serve an agent over ACP HTTP/SSE and WebSocket.
    Serve {
        agent_id: String,
        /// Hostname or IP address for the HTTP listener.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// TCP port for the HTTP listener. Use 0 for an ephemeral port.
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// ACP HTTP and WebSocket endpoint path.
        #[arg(long, default_value = "/acp")]
        path: String,
        /// Browser origin allowed to access the endpoint. May be repeated.
        #[arg(
            long = "cors-origin",
            value_name = "ORIGIN",
            conflicts_with = "allow_any_origin"
        )]
        cors_origins: Vec<String>,
        /// Allow requests from every browser origin.
        #[arg(long, conflicts_with = "cors_origins")]
        allow_any_origin: bool,
        /// Disable the GET /health endpoint.
        #[arg(long)]
        no_health: bool,
        /// Arguments passed to the agent process.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Search agents by ID, name, or description.
    Search { query: String },
}

/// Process outcome returned by a CLI command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliExit {
    /// The command completed successfully.
    Success,
    /// The command completed with a non-zero process exit code.
    Code(i32),
}

/// Dispatches a parsed CLI command.
pub async fn execute_cli<W: Write>(cli: Cli, writer: &mut W) -> anyhow::Result<CliExit> {
    match cli.command {
        Commands::RunInDir {
            current_dir,
            program,
            args,
        } => {
            let status = crate::runner::run_in_directory(&current_dir, &program, args)
                .await
                .with_context(|| {
                    format!(
                        "failed to run {} in {}",
                        program.display(),
                        current_dir.display()
                    )
                })?;
            Ok(exit_from_status(status))
        }
        Commands::List => {
            list::list_agents(writer)
                .await
                .context("failed to list registry agents")?;
            Ok(CliExit::Success)
        }
        Commands::Install { agent_id } => {
            let outcome = install::install_agent(&agent_id)
                .await
                .with_context(|| format!("failed to install agent \"{agent_id}\""))?;
            writeln!(writer, "{outcome}")?;
            Ok(CliExit::Success)
        }
        Commands::InstallEnv { yes } => {
            crate::installer::environment::install_env(writer, yes)
                .await
                .context("failed to install environment dependencies")?;
            Ok(CliExit::Success)
        }
        Commands::Run { agent_id, args } => {
            let status = run::run_agent(&agent_id, &args)
                .await
                .with_context(|| format!("failed to run agent \"{agent_id}\""))?;
            Ok(exit_from_status(status))
        }
        Commands::Serve {
            agent_id,
            host,
            port,
            path,
            cors_origins,
            allow_any_origin,
            no_health,
            args,
        } => serve::serve_agent(
            &agent_id,
            serve::ServeOptions {
                host,
                port,
                path,
                cors: serve::cors_options(cors_origins, allow_any_origin)?,
                health_endpoint: !no_health,
            },
            &args,
        )
        .await
        .with_context(|| format!("failed to serve agent \"{agent_id}\""))
        .map(|()| CliExit::Success),
        Commands::Search { query } => {
            search::search_agents(&query, writer)
                .await
                .with_context(|| format!("failed to search registry agents for \"{query}\""))?;
            Ok(CliExit::Success)
        }
    }
}

fn exit_from_status(status: ExitStatus) -> CliExit {
    if status.success() {
        return CliExit::Success;
    }
    status
        .code()
        .map_or_else(|| CliExit::Code(signal_exit_code(status)), CliExit::Code)
}

#[cfg(unix)]
fn signal_exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;

    status.signal().map_or(1, |signal| 128 + signal)
}

#[cfg(not(unix))]
fn signal_exit_code(_: ExitStatus) -> i32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_install_env_yes_flag() {
        let cli = Cli::try_parse_from(["acp-agent", "install-env", "--yes"]).unwrap();
        assert!(matches!(cli.command, Commands::InstallEnv { yes: true }));
    }

    #[test]
    fn parses_run_subcommand_and_agent_arguments() {
        let cli = Cli::try_parse_from(["acp-agent", "run", "demo", "--model", "gpt-5"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Run { agent_id, args }
                if agent_id == "demo" && args == ["--model", "gpt-5"]
        ));
    }

    #[test]
    fn parses_internal_working_directory_wrapper_arguments() {
        let cli = Cli::try_parse_from([
            "acp-agent",
            "__run-in-dir",
            "/cache/demo",
            "/cache/demo/bin/agent",
            "--stdio",
            "--model",
            "gpt-5",
        ])
        .unwrap();

        assert!(matches!(
            cli.command,
            Commands::RunInDir {
                current_dir,
                program,
                args,
            } if current_dir == std::path::Path::new("/cache/demo")
                && program == std::path::Path::new("/cache/demo/bin/agent")
                && args == ["--stdio", "--model", "gpt-5"]
        ));
    }

    #[test]
    fn parses_serve_subcommand_and_agent_arguments() {
        let cli = Cli::try_parse_from([
            "acp-agent",
            "serve",
            "demo",
            "--host",
            "0.0.0.0",
            "--port",
            "8010",
            "--path",
            "/rpc",
            "--cors-origin",
            "https://example.com",
            "--no-health",
            "--",
            "--model",
            "gpt-5",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Commands::Serve {
                agent_id,
                host,
                port,
                path,
                cors_origins,
                no_health,
                args,
                ..
            }
                if agent_id == "demo"
                    && host == "0.0.0.0"
                    && port == 8010
                    && path == "/rpc"
                    && cors_origins == ["https://example.com"]
                    && no_health
                    && args == ["--model", "gpt-5"]
        ));
    }

    #[test]
    fn parses_serve_defaults() {
        let cli = Cli::try_parse_from(["acp-agent", "serve", "demo"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Serve {
                host,
                port,
                path,
                cors_origins,
                allow_any_origin,
                no_health,
                ..
            } if host == "127.0.0.1"
                && port == 0
                && path == "/acp"
                && cors_origins.is_empty()
                && !allow_any_origin
                && !no_health
        ));
    }

    #[test]
    fn rejects_conflicting_cors_options() {
        let error = Cli::try_parse_from([
            "acp-agent",
            "serve",
            "demo",
            "--cors-origin",
            "https://example.com",
            "--allow-any-origin",
        ])
        .unwrap_err();

        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}
