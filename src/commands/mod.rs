use std::io::Write;
use std::process::ExitStatus;

use anyhow::Context;
use clap::{Parser, Subcommand};

pub mod install;
pub mod install_env;
pub mod list;
pub mod run;
pub mod search;

#[derive(Debug, Parser)]
#[command(
    name = "acp-agent",
    version,
    about = "Install, discover, and run ACP agents from the public registry."
)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
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
    #[command(trailing_var_arg = true)]
    Run {
        agent_id: String,
        #[arg(long, default_value = "stdio", help = "stdio or single-session HTTP/2 byte stream")]
        transport: run::RunTransport,
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 0)]
        port: u16,
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliExit {
    Success,
    Code(i32),
}

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
        Commands::Run {
            agent_id,
            transport,
            host,
            port,
            args,
        } => {
            let status = run::run_agent(
                &agent_id,
                run::RunOptions {
                    transport,
                    host,
                    port,
                },
                &args,
            )
            .await
            .with_context(|| format!("failed to run agent \"{agent_id}\""))?;
            Ok(exit_from_status(status))
        }
    }
}

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
    fn parses_run_subcommand_with_default_stdio_transport() {
        let cli = Cli::try_parse_from(["acp-agent", "run", "demo-agent", "--", "--model", "gpt-5"])
            .unwrap();

        match cli.command {
            Commands::Run {
                agent_id,
                transport,
                host,
                port,
                args,
            } => {
                assert_eq!(agent_id, "demo-agent");
                assert_eq!(transport, run::RunTransport::Stdio);
                assert_eq!(host, "127.0.0.1");
                assert_eq!(port, 0);
                assert_eq!(args, vec!["--model", "gpt-5"]);
            }
            command => panic!("unexpected command: {command:?}"),
        }
    }

    #[test]
    fn parses_run_subcommand_with_explicit_stdio_transport() {
        let cli = Cli::try_parse_from([
            "acp-agent",
            "run",
            "demo-agent",
            "--transport",
            "stdio",
            "--host",
            "0.0.0.0",
            "--port",
            "8080",
        ])
        .unwrap();

        match cli.command {
            Commands::Run {
                transport,
                host,
                port,
                ..
            } => {
                assert_eq!(transport, run::RunTransport::Stdio);
                assert_eq!(host, "0.0.0.0");
                assert_eq!(port, 8080);
            }
            command => panic!("unexpected command: {command:?}"),
        }
    }

    #[test]
    fn parses_run_subcommand_with_http_transport() {
        let cli = Cli::try_parse_from([
            "acp-agent",
            "run",
            "demo-agent",
            "--transport",
            "http",
            "--host",
            "127.0.0.1",
            "--port",
            "9000",
        ])
        .unwrap();

        match cli.command {
            Commands::Run {
                transport,
                host,
                port,
                ..
            } => {
                assert_eq!(transport, run::RunTransport::Http);
                assert_eq!(host, "127.0.0.1");
                assert_eq!(port, 9000);
            }
            command => panic!("unexpected command: {command:?}"),
        }
    }

    #[test]
    fn rejects_archived_serve_subcommand() {
        let error = Cli::try_parse_from(["acp-agent", "serve", "demo-agent"]).unwrap_err();

        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
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
