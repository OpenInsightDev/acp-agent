use std::io::Write;
#[cfg(unix)]
use std::path::PathBuf;
use std::process::ExitStatus;

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::runtime::serve::ServeMode;

const DEFAULT_SERVE_HOST: &str = "127.0.0.1";
const DEFAULT_SERVE_PORT: u16 = 0;

/// Agent installation entrypoints and install outcome types.
pub mod install;
/// Runtime dependency detection and optional bootstrap installers.
pub mod install_env;
/// Registry listing output helpers.
pub mod list;
/// Stdio execution helpers for launching an agent directly.
pub mod run;
/// Registry search output helpers.
pub mod search;
/// Network-serving helpers that wrap the runtime transport layer.
pub mod serve;

/// High-level CLI entrypoint shared by the binaries in this crate.
///
/// `acp-agent` exposes discover/install/run/serve operations for agents
/// hosted in the public registry. The CLI delegates the actual work to
/// the helpers defined in the sibling modules so that tests can exercise them
/// programmatically.

/// CLI arguments consumed by the `acp-agent` binary.
///
/// The parser is intentionally thin: it only captures which subcommand the user
/// invoked so that `execute_cli` can route to the appropriate handler and keep
/// the binary minimal while still exposing helpers for other callers.
#[derive(Debug, Parser)]
#[command(
    name = "acp-agent",
    version,
    about = "Install, discover, run, and serve ACP agents from the public registry."
)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ServeTransportArg {
    Http,
    Tcp,
    Ws,
    #[cfg(unix)]
    Uds,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
struct ServeCliOptions {
    #[arg(
        long,
        default_value = "http",
        help = "Network transport to expose (http, tcp, ws, or uds on Unix)."
    )]
    transport: ServeTransportArg,
    #[cfg_attr(unix, arg(conflicts_with = "unix_socket"))]
    #[arg(long)]
    host: Option<String>,
    #[cfg_attr(unix, arg(conflicts_with = "unix_socket"))]
    #[arg(long)]
    port: Option<u16>,
    #[cfg(unix)]
    #[arg(long = "unix-socket", required_if_eq("transport", "uds"))]
    unix_socket: Option<PathBuf>,
}

impl ServeCliOptions {
    fn into_mode(self) -> anyhow::Result<ServeMode> {
        match self.transport {
            ServeTransportArg::Http => {
                #[cfg(unix)]
                reject_unix_socket_for_non_uds(&self.unix_socket)?;

                Ok(ServeMode::Http {
                    host: self.host.unwrap_or_else(|| DEFAULT_SERVE_HOST.to_string()),
                    port: self.port.unwrap_or(DEFAULT_SERVE_PORT),
                })
            }
            ServeTransportArg::Tcp => {
                #[cfg(unix)]
                reject_unix_socket_for_non_uds(&self.unix_socket)?;

                Ok(ServeMode::Tcp {
                    host: self.host.unwrap_or_else(|| DEFAULT_SERVE_HOST.to_string()),
                    port: self.port.unwrap_or(DEFAULT_SERVE_PORT),
                })
            }
            ServeTransportArg::Ws => {
                #[cfg(unix)]
                reject_unix_socket_for_non_uds(&self.unix_socket)?;

                Ok(ServeMode::Ws {
                    host: self.host.unwrap_or_else(|| DEFAULT_SERVE_HOST.to_string()),
                    port: self.port.unwrap_or(DEFAULT_SERVE_PORT),
                })
            }
            #[cfg(unix)]
            ServeTransportArg::Uds => {
                if self.host.is_some() || self.port.is_some() {
                    bail!("--transport uds cannot be used with --host or --port");
                }

                let path = self
                    .unix_socket
                    .context("--transport uds requires --unix-socket <path>")?;
                Ok(ServeMode::Uds { path })
            }
        }
    }
}

#[cfg(unix)]
fn reject_unix_socket_for_non_uds(unix_socket: &Option<PathBuf>) -> anyhow::Result<()> {
    if unix_socket.is_some() {
        bail!("--unix-socket can only be used with --transport uds");
    }

    Ok(())
}

#[derive(Debug, Subcommand)]
enum Commands {
    List,
    Search {
        agent_id: String,
    },
    InstallEnv {
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
    Install {
        agent_id: String,
    },
    #[command(about = "Run an ACP agent over stdio.")]
    Run {
        agent_id: String,
        #[arg(help = "Arguments passed through to the agent process.")]
        args: Vec<String>,
    },
    #[command(
        about = "Serve an ACP agent over a network transport.",
        trailing_var_arg = true
    )]
    Serve {
        agent_id: String,
        #[command(flatten)]
        options: ServeCliOptions,
        #[arg(help = "Arguments passed through to the agent process.")]
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// Normalized exit status returned by `execute_cli`.
///
/// CLI callers still receive an `anyhow::Result`, but this enum represents the
/// exit code that should be returned to the OS if `execute_cli` succeeds. A
/// `Success` value maps to `0`, whereas `Code` preserves a non-zero process
/// status (including signals on Unix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliExit {
    /// The command completed successfully and should exit with code `0`.
    Success,
    /// The command completed and wants the process to exit with the provided code.
    Code(i32),
}

/// Dispatches the parsed `Cli` to the concrete command handlers.
///
/// The function mirrors the binary’s subcommand list so that the CLI can be
/// exercised from tests or other binaries by composing `Cli` + a writer that,
/// for example, records output instead of writing to `stdout`.
pub async fn execute_cli<W: Write>(cli: Cli, writer: &mut W) -> anyhow::Result<CliExit> {
    match cli.command {
        Commands::List => {
            list::list_agents(writer)
                .await
                .with_context(|| "failed to list registry agents".to_string())?;
            Ok(CliExit::Success)
        }
        Commands::Search { agent_id } => {
            search::search_agents(&agent_id, writer)
                .await
                .with_context(|| format!("failed to search registry agents for \"{agent_id}\""))?;
            Ok(CliExit::Success)
        }
        Commands::InstallEnv { yes } => {
            install_env::install_env(writer, yes)
                .await
                .with_context(|| "failed to install environment dependencies".to_string())?;
            Ok(CliExit::Success)
        }
        Commands::Install { agent_id } => {
            let outcome = install::install_agent(&agent_id)
                .await
                .with_context(|| format!("failed to install agent \"{agent_id}\""))?;
            writeln!(writer, "{outcome}")?;
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
            options,
            args,
        } => {
            let mode = options.into_mode()?;
            let status = serve::serve_agent(&agent_id, mode, &args)
                .await
                .with_context(|| format!("failed to serve agent \"{agent_id}\""))?;
            Ok(exit_from_status(status))
        }
    }
}

/// Converts process `ExitStatus` into the API-friendly `CliExit` representation.
fn exit_from_status(status: ExitStatus) -> CliExit {
    if status.success() {
        return CliExit::Success;
    }

    if let Some(code) = status.code() {
        return CliExit::Code(code);
    }

    CliExit::Code(signal_exit_code(status))
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
    use clap::error::ErrorKind;

    #[test]
    fn parses_install_subcommand() {
        let cli = Cli::try_parse_from(["acp-agent", "install", "demo-agent"]).unwrap();

        match cli.command {
            Commands::Install { agent_id } => assert_eq!(agent_id, "demo-agent"),
            command => panic!("unexpected command: {command:?}"),
        }
    }

    #[test]
    fn parses_list_subcommand() {
        let cli = Cli::try_parse_from(["acp-agent", "list"]).unwrap();

        match cli.command {
            Commands::List => {}
            command => panic!("unexpected command: {command:?}"),
        }
    }

    #[test]
    fn parses_search_subcommand() {
        let cli = Cli::try_parse_from(["acp-agent", "search", "demo"]).unwrap();

        match cli.command {
            Commands::Search { agent_id } => assert_eq!(agent_id, "demo"),
            command => panic!("unexpected command: {command:?}"),
        }
    }

    #[test]
    fn parses_install_env_subcommand() {
        let cli = Cli::try_parse_from(["acp-agent", "install-env"]).unwrap();

        match cli.command {
            Commands::InstallEnv { yes } => assert!(!yes),
            command => panic!("unexpected command: {command:?}"),
        }
    }

    #[test]
    fn parses_install_env_subcommand_with_yes_flag() {
        let cli = Cli::try_parse_from(["acp-agent", "install-env", "-y"]).unwrap();

        match cli.command {
            Commands::InstallEnv { yes } => assert!(yes),
            command => panic!("unexpected command: {command:?}"),
        }
    }

    #[test]
    fn parses_run_subcommand_with_model_args() {
        let cli = Cli::try_parse_from(["acp-agent", "run", "demo-agent", "--", "--model", "gpt-5"])
            .unwrap();

        match cli.command {
            Commands::Run { agent_id, args } => {
                assert_eq!(agent_id, "demo-agent");
                assert_eq!(args, vec!["--model", "gpt-5"]);
            }
            command => panic!("unexpected command: {command:?}"),
        }
    }

    #[test]
    fn parses_serve_subcommand_with_defaults() {
        let cli = Cli::try_parse_from(["acp-agent", "serve", "demo-agent"]).unwrap();

        match cli.command {
            Commands::Serve {
                agent_id,
                options,
                args,
            } => {
                assert_eq!(agent_id, "demo-agent");
                assert_eq!(
                    options.into_mode().unwrap(),
                    ServeMode::Http {
                        host: DEFAULT_SERVE_HOST.to_string(),
                        port: DEFAULT_SERVE_PORT,
                    }
                );
                assert!(args.is_empty());
            }
            command => panic!("unexpected command: {command:?}"),
        }
    }

    #[test]
    fn parses_serve_subcommand_with_explicit_options() {
        let cli = Cli::try_parse_from([
            "acp-agent",
            "serve",
            "demo-agent",
            "--transport",
            "ws",
            "--host",
            "0.0.0.0",
            "--port",
            "8010",
            "--",
            "--model",
            "gpt-6",
        ])
        .unwrap();

        match cli.command {
            Commands::Serve { options, args, .. } => {
                assert_eq!(
                    options.into_mode().unwrap(),
                    ServeMode::Ws {
                        host: "0.0.0.0".to_string(),
                        port: 8010,
                    }
                );
                assert_eq!(args, vec!["--model", "gpt-6"]);
            }
            command => panic!("unexpected command: {command:?}"),
        }
    }

    #[test]
    fn parses_serve_subcommand_with_tcp_transport() {
        let cli = Cli::try_parse_from(["acp-agent", "serve", "demo-agent", "--transport", "tcp"])
            .unwrap();

        match cli.command {
            Commands::Serve { options, .. } => {
                assert_eq!(
                    options.into_mode().unwrap(),
                    ServeMode::Tcp {
                        host: DEFAULT_SERVE_HOST.to_string(),
                        port: DEFAULT_SERVE_PORT,
                    }
                );
            }
            command => panic!("unexpected command: {command:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn parses_serve_subcommand_with_uds_transport() {
        let cli = Cli::try_parse_from([
            "acp-agent",
            "serve",
            "demo-agent",
            "--transport",
            "uds",
            "--unix-socket",
            "/tmp/acp-agent.sock",
        ])
        .unwrap();

        match cli.command {
            Commands::Serve { options, .. } => {
                assert_eq!(
                    options.into_mode().unwrap(),
                    ServeMode::Uds {
                        path: PathBuf::from("/tmp/acp-agent.sock"),
                    }
                );
            }
            command => panic!("unexpected command: {command:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_uds_with_host_or_port() {
        let error = Cli::try_parse_from([
            "acp-agent",
            "serve",
            "demo-agent",
            "--transport",
            "uds",
            "--unix-socket",
            "/tmp/acp-agent.sock",
            "--host",
            "127.0.0.1",
        ])
        .unwrap_err();

        assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_missing_unix_socket_for_uds() {
        let error = Cli::try_parse_from(["acp-agent", "serve", "demo-agent", "--transport", "uds"])
            .unwrap_err();

        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_unix_socket_for_non_uds_transport() {
        let cli = Cli::try_parse_from([
            "acp-agent",
            "serve",
            "demo-agent",
            "--transport",
            "tcp",
            "--unix-socket",
            "/tmp/acp-agent.sock",
        ])
        .unwrap();

        match cli.command {
            Commands::Serve { options, .. } => {
                let error = options.into_mode().unwrap_err();
                assert_eq!(
                    error.to_string(),
                    "--unix-socket can only be used with --transport uds"
                );
            }
            command => panic!("unexpected command: {command:?}"),
        }
    }

    #[test]
    fn rejects_serve_subcommand_with_stdio_transport() {
        let error =
            Cli::try_parse_from(["acp-agent", "serve", "demo-agent", "--transport", "stdio"])
                .unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvalidValue);
    }

    #[test]
    fn exit_from_status_returns_success_for_zero_exit() {
        assert_eq!(exit_from_status(success_exit_status()), CliExit::Success);
    }

    #[test]
    fn exit_from_status_returns_process_code_for_non_zero_exit() {
        assert_eq!(exit_from_status(exit_status_with_code(5)), CliExit::Code(5));
    }

    #[cfg(unix)]
    #[test]
    fn exit_from_status_returns_signal_convention_for_signal_exit() {
        assert_eq!(exit_from_status(signal_exit_status(15)), CliExit::Code(143));
    }

    fn success_exit_status() -> ExitStatus {
        exit_status_with_code(0)
    }

    #[cfg(unix)]
    fn exit_status_with_code(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;

        ExitStatus::from_raw(code << 8)
    }

    #[cfg(windows)]
    fn exit_status_with_code(code: i32) -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;

        ExitStatus::from_raw(code as u32)
    }

    #[cfg(unix)]
    fn signal_exit_status(signal: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;

        ExitStatus::from_raw(signal)
    }
}
