use std::fmt::Display;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitStatus;

use anyhow::Context;
use clap::{Parser, Subcommand};

/// Local cache management commands (`list --installed`, `uninstall`, `update`).
pub mod cache;
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
/// Named ACP server management commands.
pub mod server;

/// Output format for registry listing and search commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentOutputFormat {
    /// Human-readable tab-separated output.
    Tsv,
    /// Full records as a pretty-printed JSON array.
    Json,
}

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
    /// Internal foreground process for a named ACP server.
    #[command(name = "__server-run", hide = true)]
    ServerRun {
        #[arg(long)]
        name: String,
        #[arg(long)]
        host: String,
        #[arg(long)]
        port: u16,
    },
    /// List every published agent.
    List {
        /// List agents cached locally instead of the published registry.
        #[arg(long)]
        installed: bool,
        /// Emit the full agent records as a pretty-printed JSON array.
        #[arg(long)]
        json: bool,
    },
    /// Install one or more agents from their preferred registry distributions.
    ///
    /// Multiple agent IDs are installed concurrently.
    Install {
        /// IDs of the agents to install.
        #[arg(value_name = "AGENT_ID", required = true)]
        agent_id: Vec<String>,
    },
    /// Remove one or more installed agents from the local cache and/or package managers.
    Uninstall {
        /// IDs of the agents to uninstall.
        #[arg(value_name = "AGENT_ID", required = true)]
        agent_id: Vec<String>,
    },
    /// Update one or more installed agents to the registry's latest distribution.
    Update {
        /// IDs of the agents to update.
        #[arg(value_name = "AGENT_ID", required = true)]
        agent_id: Vec<String>,
    },
    /// Install Deno or uv when no compatible local toolchain exists.
    InstallEnv {
        /// Skip the confirmation prompt.
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
    /// Run an agent locally over stdio.
    Run {
        agent_id: String,
        /// Activate the agent's yolo/auto-approve mode (injects the mapped startup flag).
        #[arg(long)]
        yolo: bool,
        /// Arguments passed to the agent process. Hyphen-prefixed arguments
        /// must come after the `--` separator.
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
        /// Optional URL prefix applied to all served endpoints (ACP, health,
        /// readyz), e.g. `/myapp` makes the ACP endpoint `/myapp/acp`.
        #[arg(long)]
        subpath: Option<String>,
        /// Use the agent id as the subpath (equivalent to `--subpath /<agent-id>`).
        #[arg(long, conflicts_with = "subpath")]
        agent_sub_path: bool,
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
        /// Disable the GET /readyz agent readiness endpoint.
        #[arg(long)]
        no_readyz: bool,
        /// Activate the agent's yolo/auto-approve mode (injects the mapped startup flag).
        #[arg(long)]
        yolo: bool,
        /// Arguments passed to the agent process.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Manage named ACP servers and their agent routes.
    Server {
        #[command(subcommand)]
        command: ServerCommands,
    },
    /// Search agents by ID, name, or description.
    Search {
        query: String,
        /// Emit the full matching agent records as a pretty-printed JSON array.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ServerCommands {
    /// Start a named ACP server in the background.
    Start {
        /// Local server name used by later commands.
        #[arg(long, default_value = "default")]
        name: String,
        /// Hostname or IP address for the named server listener.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// TCP port for the named server listener. Use 0 for an ephemeral port.
        #[arg(long, default_value_t = 8010)]
        port: u16,
    },
    /// Stop a named ACP server.
    Stop {
        /// Local server name.
        #[arg(long, default_value = "default")]
        name: String,
    },
    /// Register an agent route with a named server.
    Register {
        agent_id: String,
        /// Target server name.
        #[arg(long, default_value = "default")]
        name: String,
        /// Public route prefix. Defaults to `/<agent-id>`.
        #[arg(long, alias = "subpath")]
        route: Option<String>,
        /// ACP HTTP and WebSocket endpoint path below the public route.
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
        /// Disable the agent's GET /health endpoint.
        #[arg(long)]
        no_health: bool,
        /// Disable the agent's GET /readyz endpoint.
        #[arg(long)]
        no_readyz: bool,
        /// Activate the agent's yolo/auto-approve mode.
        #[arg(long)]
        yolo: bool,
        /// Arguments passed to the agent process.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Remove an agent route from a named server.
    Unregister {
        agent_id: String,
        /// Target server name.
        #[arg(long, default_value = "default")]
        name: String,
    },
    /// List named servers and their process states.
    List {
        /// Emit server records as structured JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show the state of a named server.
    Status {
        /// Local server name.
        #[arg(long, default_value = "default")]
        name: String,
        /// Emit the server record as structured JSON.
        #[arg(long)]
        json: bool,
    },
    /// List the agent routes registered with a named server.
    Registrations {
        /// Local server name.
        #[arg(long, default_value = "default")]
        name: String,
        /// Emit registration records as structured JSON.
        #[arg(long)]
        json: bool,
    },
    /// Tail a named server's log with control tokens and credentials redacted.
    Logs {
        /// Local server name.
        #[arg(long, default_value = "default")]
        name: String,
        /// Number of log lines to tail.
        #[arg(long, default_value_t = 50)]
        lines: usize,
        /// Emit the log lines as structured JSON.
        #[arg(long)]
        json: bool,
    },
}

/// Process outcome returned by a CLI command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliExit {
    /// The command completed successfully.
    Success,
    /// The command completed with a non-zero process exit code.
    Code(i32),
}

/// Reports the outcome of a multi-agent operation with all-or-nothing
/// semantics, matching the convention of npm and other package managers.
///
/// Success is only reported (and `CliExit::Success` returned) when every
/// requested agent succeeded. If any agent failed, the failures are printed
/// and a non-zero exit is returned; the command never reports overall success
/// when part of the batch failed.
fn report_batch_outcome<W, T>(
    writer: &mut W,
    outcomes: &[(String, anyhow::Result<T>)],
    action: &str,
) -> anyhow::Result<CliExit>
where
    W: Write,
    T: Display,
{
    if outcomes.iter().all(|(_, result)| result.is_ok()) {
        for (_, result) in outcomes {
            if let Ok(outcome) = result {
                writeln!(writer, "{outcome}")?;
            }
        }
        return Ok(CliExit::Success);
    }

    for (id, result) in outcomes {
        if let Err(error) = result {
            writeln!(writer, "failed to {action} agent \"{id}\": {error:#}")?;
        }
    }
    Ok(CliExit::Code(1))
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
        Commands::ServerRun { name, host, port } => {
            server::run(name, host, port).await?;
            Ok(CliExit::Success)
        }
        Commands::List { installed, json } => {
            let format = if json {
                AgentOutputFormat::Json
            } else {
                AgentOutputFormat::Tsv
            };
            if installed {
                cache::list_installed_with_format(writer, format)
                    .await
                    .context("failed to list installed agents")?;
            } else {
                list::list_agents_with_format(writer, format)
                    .await
                    .context("failed to list registry agents")?;
            }
            Ok(CliExit::Success)
        }
        Commands::Install { agent_id } => {
            let outcomes = install::install_agents(&agent_id).await;
            report_batch_outcome(writer, &outcomes, "install")
        }
        Commands::Uninstall { agent_id } => {
            let outcomes = cache::uninstall_agents(&agent_id).await;
            report_batch_outcome(writer, &outcomes, "uninstall")
        }
        Commands::Update { agent_id } => {
            let outcomes = cache::update_agents(&agent_id).await;
            report_batch_outcome(writer, &outcomes, "update")
        }
        Commands::InstallEnv { yes } => {
            crate::installer::environment::install_env(writer, yes)
                .await
                .context("failed to install environment dependencies")?;
            Ok(CliExit::Success)
        }
        Commands::Run {
            agent_id,
            args,
            yolo,
        } => {
            let args = resolve_yolo_args(&agent_id, yolo, args).await?;
            let status = run::run_agent(&agent_id, &args)
                .await
                .with_context(|| format!("failed to run agent \"{agent_id}\""))?;
            Ok(exit_from_status(status))
        }
        Commands::Serve {
            agent_id,
            host,
            port,
            subpath,
            agent_sub_path,
            path,
            cors_origins,
            allow_any_origin,
            no_health,
            no_readyz,
            yolo,
            args,
        } => {
            let args = resolve_yolo_args(&agent_id, yolo, args).await?;
            let subpath = resolve_subpath(&agent_id, subpath, agent_sub_path);
            serve::serve_agent(
                &agent_id,
                serve::ServeOptions {
                    host,
                    port,
                    subpath,
                    path,
                    cors: serve::cors_options(cors_origins, allow_any_origin)?,
                    health_endpoint: !no_health,
                    readyz_endpoint: !no_readyz,
                },
                &args,
            )
            .await
            .with_context(|| format!("failed to serve agent \"{agent_id}\""))
            .map(|()| CliExit::Success)
        }
        Commands::Server { command } => match command {
            ServerCommands::Start { name, host, port } => {
                let message = server::start(server::StartOptions { name, host, port }).await?;
                writeln!(writer, "{message}")?;
                Ok(CliExit::Success)
            }
            ServerCommands::Stop { name } => {
                let message = server::stop(&name).await?;
                writeln!(writer, "{message}")?;
                Ok(CliExit::Success)
            }
            ServerCommands::Register {
                agent_id,
                name,
                route,
                path,
                cors_origins,
                allow_any_origin,
                no_health,
                no_readyz,
                yolo,
                args,
            } => {
                let message = server::register(
                    &agent_id,
                    server::RegisterOptions {
                        name,
                        route,
                        path,
                        cors_origins,
                        allow_any_origin,
                        health_endpoint: !no_health,
                        readyz_endpoint: !no_readyz,
                        yolo,
                        args,
                    },
                )
                .await?;
                writeln!(writer, "{message}")?;
                Ok(CliExit::Success)
            }
            ServerCommands::Unregister { agent_id, name } => {
                let message = server::unregister(&agent_id, &name).await?;
                writeln!(writer, "{message}")?;
                Ok(CliExit::Success)
            }
            ServerCommands::List { json } => {
                server::list(writer, json)
                    .await
                    .context("failed to list named servers")?;
                Ok(CliExit::Success)
            }
            ServerCommands::Status { name, json } => {
                server::status(writer, &name, json)
                    .await
                    .with_context(|| format!("failed to inspect server \"{name}\""))?;
                Ok(CliExit::Success)
            }
            ServerCommands::Registrations { name, json } => {
                server::registrations(writer, &name, json)
                    .await
                    .with_context(|| {
                        format!("failed to list registrations for server \"{name}\"")
                    })?;
                Ok(CliExit::Success)
            }
            ServerCommands::Logs { name, lines, json } => {
                server::logs(writer, &name, lines, json)
                    .await
                    .with_context(|| format!("failed to read logs for server \"{name}\""))?;
                Ok(CliExit::Success)
            }
        },
        Commands::Search { query, json } => {
            let format = if json {
                AgentOutputFormat::Json
            } else {
                AgentOutputFormat::Tsv
            };
            search::search_agents_with_format(&query, writer, format)
                .await
                .with_context(|| format!("failed to search registry agents for \"{query}\""))?;
            Ok(CliExit::Success)
        }
    }
}

/// Resolves the effective served subpath, deriving `/` + agent id when
/// `--agent-sub-path` is set (equivalent to `--subpath /<agent-id>`).
fn resolve_subpath(
    agent_id: &str,
    subpath: Option<String>,
    agent_sub_path: bool,
) -> Option<String> {
    if agent_sub_path {
        Some(format!("/{agent_id}"))
    } else {
        subpath
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

/// Prepends the agent's yolo startup flag when `--yolo` was requested.
///
/// Resolution fails loudly (rather than silently skipping the requested
/// auto-approve behavior) when the agent only supports protocol-level yolo.
async fn resolve_yolo_args(
    agent_id: &str,
    yolo: bool,
    args: Vec<String>,
) -> anyhow::Result<Vec<String>> {
    if !yolo {
        return Ok(args);
    }

    let extra = crate::yolo::yolo_extra_args(agent_id).await?;
    Ok(extra.into_iter().chain(args).collect())
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
    fn batch_outcome_is_all_or_nothing_success() {
        let mut output = Vec::new();
        let outcomes = vec![
            (
                "a".to_string(),
                Ok::<_, anyhow::Error>("installed a".to_string()),
            ),
            (
                "b".to_string(),
                Ok::<_, anyhow::Error>("installed b".to_string()),
            ),
        ];

        let exit = report_batch_outcome(&mut output, &outcomes, "install").unwrap();

        assert!(matches!(exit, CliExit::Success));
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "installed a\ninstalled b\n"
        );
    }

    #[test]
    fn batch_outcome_fails_when_any_agent_fails() {
        let mut output = Vec::new();
        let outcomes = vec![
            (
                "a".to_string(),
                Ok::<_, anyhow::Error>("installed a".to_string()),
            ),
            ("b".to_string(), Err::<String, _>(anyhow::anyhow!("boom"))),
        ];

        let exit = report_batch_outcome(&mut output, &outcomes, "install").unwrap();

        assert!(matches!(exit, CliExit::Code(1)));
        // Success lines are not printed when the batch failed.
        assert!(
            !String::from_utf8(output.clone())
                .unwrap()
                .contains("installed a")
        );
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("failed to install agent \"b\": boom")
        );
    }

    #[test]
    fn install_requires_at_least_one_agent() {
        let error = Cli::try_parse_from(["acp-agent", "install"]).unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn uninstall_and_update_require_at_least_one_agent() {
        let error = Cli::try_parse_from(["acp-agent", "uninstall"]).unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );

        let error = Cli::try_parse_from(["acp-agent", "update"]).unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn parses_run_subcommand_and_agent_arguments() {
        let cli =
            Cli::try_parse_from(["acp-agent", "run", "demo", "--", "--model", "gpt-5"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Run {
                agent_id,
                args,
                yolo,
            } if agent_id == "demo" && args == ["--model", "gpt-5"] && !yolo
        ));
    }

    #[test]
    fn parses_run_subcommand_with_yolo_flag() {
        let cli = Cli::try_parse_from(["acp-agent", "run", "demo", "--yolo"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Run {
                agent_id,
                yolo,
                ..
            } if agent_id == "demo" && yolo
        ));
    }

    #[test]
    fn rejects_hyphenated_run_args_without_separator() {
        let error =
            Cli::try_parse_from(["acp-agent", "run", "demo", "--model", "gpt-5"]).unwrap_err();

        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
        let message = error.to_string();
        assert!(
            message.contains("--"),
            "expected clap to hint at the `--` separator, got: {message}"
        );
    }

    #[test]
    fn list_and_search_default_to_tsv_output() {
        let list = Cli::try_parse_from(["acp-agent", "list"]).unwrap();
        assert!(matches!(list.command, Commands::List { json: false, .. }));

        let search = Cli::try_parse_from(["acp-agent", "search", "helper"]).unwrap();
        assert!(matches!(
            search.command,
            Commands::Search { json: false, .. }
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
    fn parses_server_commands_and_defaults() {
        let start = Cli::try_parse_from(["acp-agent", "server", "start"]).unwrap();
        assert!(matches!(
            start.command,
            Commands::Server {
                command: ServerCommands::Start { name, host, port }
            } if name == "default" && host == "127.0.0.1" && port == 8010
        ));

        let stop = Cli::try_parse_from(["acp-agent", "server", "stop", "--name", "work"]).unwrap();
        assert!(matches!(
            stop.command,
            Commands::Server {
                command: ServerCommands::Stop { name }
            } if name == "work"
        ));
    }

    #[test]
    fn parses_server_register_options_and_agent_arguments() {
        let cli = Cli::try_parse_from([
            "acp-agent",
            "server",
            "register",
            "demo",
            "--name",
            "work",
            "--route",
            "/assistant",
            "--path",
            "/rpc",
            "--yolo",
            "--",
            "--model",
            "gpt-5",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Commands::Server {
                command: ServerCommands::Register {
                    agent_id,
                    name,
                    route,
                    path,
                    yolo,
                    args,
                    ..
                }
            } if agent_id == "demo"
                && name == "work"
                && route.as_deref() == Some("/assistant")
                && path == "/rpc"
                && yolo
                && args == ["--model", "gpt-5"]
        ));

        let unregister =
            Cli::try_parse_from(["acp-agent", "server", "unregister", "demo"]).unwrap();
        assert!(matches!(
            unregister.command,
            Commands::Server {
                command: ServerCommands::Unregister { agent_id, name }
            } if agent_id == "demo" && name == "default"
        ));
    }

    #[test]
    fn parses_server_inspection_commands_and_defaults() {
        let list = Cli::try_parse_from(["acp-agent", "server", "list"]).unwrap();
        assert!(matches!(
            list.command,
            Commands::Server {
                command: ServerCommands::List { json: false }
            }
        ));
        let list = Cli::try_parse_from(["acp-agent", "server", "list", "--json"]).unwrap();
        assert!(matches!(
            list.command,
            Commands::Server {
                command: ServerCommands::List { json: true }
            }
        ));

        let status = Cli::try_parse_from(["acp-agent", "server", "status"]).unwrap();
        assert!(matches!(
            status.command,
            Commands::Server {
                command: ServerCommands::Status { name, json: false }
            } if name == "default"
        ));
        let status =
            Cli::try_parse_from(["acp-agent", "server", "status", "--name", "work", "--json"])
                .unwrap();
        assert!(matches!(
            status.command,
            Commands::Server {
                command: ServerCommands::Status { name, json: true }
            } if name == "work"
        ));

        let registrations =
            Cli::try_parse_from(["acp-agent", "server", "registrations", "--name", "work"])
                .unwrap();
        assert!(matches!(
            registrations.command,
            Commands::Server {
                command: ServerCommands::Registrations { name, json: false }
            } if name == "work"
        ));

        let logs = Cli::try_parse_from([
            "acp-agent",
            "server",
            "logs",
            "--name",
            "work",
            "--lines",
            "100",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            logs.command,
            Commands::Server {
                command: ServerCommands::Logs { name, lines, json: true }
            } if name == "work" && lines == 100
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
            "--subpath",
            "/myapp",
            "--cors-origin",
            "https://example.com",
            "--no-health",
            "--no-readyz",
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
                subpath,
                path,
                cors_origins,
                no_health,
                no_readyz,
                args,
                ..
            }
                if agent_id == "demo"
                    && host == "0.0.0.0"
                    && port == 8010
                    && subpath.as_deref() == Some("/myapp")
                    && path == "/rpc"
                    && cors_origins == ["https://example.com"]
                    && no_health
                    && no_readyz
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
                subpath,
                path,
                cors_origins,
                allow_any_origin,
                no_health,
                no_readyz,
                yolo,
                ..
            } if host == "127.0.0.1"
                && port == 0
                && subpath.is_none()
                && path == "/acp"
                && cors_origins.is_empty()
                && !allow_any_origin
                && !no_health
                && !no_readyz
                && !yolo
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

    #[test]
    fn parses_agent_sub_path_flag() {
        let cli =
            Cli::try_parse_from(["acp-agent", "serve", "codex-acp", "--agent-sub-path"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Serve {
                agent_id,
                subpath,
                agent_sub_path,
                ..
            } if agent_id == "codex-acp"
                && subpath.is_none()
                && agent_sub_path
        ));
    }

    #[test]
    fn rejects_agent_sub_path_with_subpath() {
        let error = Cli::try_parse_from([
            "acp-agent",
            "serve",
            "demo",
            "--agent-sub-path",
            "--subpath",
            "/x",
        ])
        .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn resolves_agent_sub_path_to_agent_id_prefix() {
        assert_eq!(
            resolve_subpath("codex-acp", None, true),
            Some("/codex-acp".to_string())
        );
        // An explicit --subpath wins over the flag (they conflict at parse,
        // but the resolver must still prefer the explicit value if reachable).
        assert_eq!(
            resolve_subpath("codex-acp", Some("/myapp".to_string()), false),
            Some("/myapp".to_string())
        );
        assert_eq!(resolve_subpath("codex-acp", None, false), None);
    }
}
