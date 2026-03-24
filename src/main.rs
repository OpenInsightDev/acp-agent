use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::process::ExitStatus;

use anyhow::Context;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "acp-agent",
    version,
    about = "Install, run, and serve ACP agents from the public registry."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Install {
        agent_id: String,
    },
    #[command(trailing_var_arg = true)]
    Run {
        agent_id: String,
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(trailing_var_arg = true)]
    Serve {
        agent_id: String,
        #[arg(long)]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type InstallHandler = dyn for<'a> Fn(&'a str) -> BoxFuture<'a, anyhow::Result<acp_agent::install::InstallOutcome>>
    + Send
    + Sync;
type RunHandler = dyn for<'a> Fn(&'a str, &'a [String]) -> BoxFuture<'a, anyhow::Result<ExitStatus>>
    + Send
    + Sync;
type ServeHandler = dyn for<'a> Fn(&'a str, &'a str, u16, &'a [String]) -> BoxFuture<'a, anyhow::Result<ExitStatus>>
    + Send
    + Sync;

struct CommandHandlers {
    install: Box<InstallHandler>,
    run: Box<RunHandler>,
    serve: Box<ServeHandler>,
}

impl CommandHandlers {
    fn production() -> Self {
        Self {
            install: Box::new(|agent_id| {
                Box::pin(async move {
                    acp_agent::install::install_agent(agent_id)
                        .await
                        .map_err(Into::into)
                })
            }),
            run: Box::new(|agent_id, args| {
                Box::pin(async move {
                    acp_agent::run::run_agent(agent_id, args)
                        .await
                        .map_err(Into::into)
                })
            }),
            serve: Box::new(|agent_id, host, port, args| {
                Box::pin(async move {
                    acp_agent::serve::serve_agent(agent_id, host, port, args)
                        .await
                        .map_err(Into::into)
                })
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliExit {
    Success,
    Code(i32),
}

async fn execute_cli<W: Write>(
    cli: Cli,
    handlers: &CommandHandlers,
    writer: &mut W,
) -> anyhow::Result<CliExit> {
    match cli.command {
        Commands::Install { agent_id } => {
            let outcome = (handlers.install)(&agent_id)
                .await
                .with_context(|| format!("failed to install agent \"{agent_id}\""))?;
            writeln!(writer, "{outcome}")?;
            Ok(CliExit::Success)
        }
        Commands::Run { agent_id, args } => {
            let status = (handlers.run)(&agent_id, &args)
                .await
                .with_context(|| format!("failed to run agent \"{agent_id}\""))?;
            Ok(exit_from_status(status))
        }
        Commands::Serve {
            agent_id,
            port,
            host,
            args,
        } => {
            let status = (handlers.serve)(&agent_id, &host, port, &args)
                .await
                .with_context(|| format!("failed to serve agent \"{agent_id}\""))?;
            Ok(exit_from_status(status))
        }
    }
}

fn exit_from_status(status: ExitStatus) -> CliExit {
    if let Some(code) = status.code() {
        if code != 0 {
            return CliExit::Code(code);
        }
    } else if !status.success() {
        return CliExit::Code(1);
    }

    CliExit::Success
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let handlers = CommandHandlers::production();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();

    match execute_cli(cli, &handlers, &mut stdout).await? {
        CliExit::Success => Ok(()),
        CliExit::Code(code) => std::process::exit(code),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use anyhow::anyhow;

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Invocation {
        Install {
            agent_id: String,
        },
        Run {
            agent_id: String,
            args: Vec<String>,
        },
        Serve {
            agent_id: String,
            host: String,
            port: u16,
            args: Vec<String>,
        },
    }

    #[test]
    fn parses_install_subcommand() {
        let cli = Cli::try_parse_from(["acp-agent", "install", "demo-agent"]).unwrap();

        match cli.command {
            Commands::Install { agent_id } => assert_eq!(agent_id, "demo-agent"),
            command => panic!("unexpected command: {command:?}"),
        }
    }

    #[test]
    fn parses_run_subcommand_with_trailing_hyphenated_args() {
        let cli = Cli::try_parse_from([
            "acp-agent",
            "run",
            "demo-agent",
            "--model",
            "gpt-5",
            "--flag",
        ])
        .unwrap();

        match cli.command {
            Commands::Run { agent_id, args } => {
                assert_eq!(agent_id, "demo-agent");
                assert_eq!(args, vec!["--model", "gpt-5", "--flag"]);
            }
            command => panic!("unexpected command: {command:?}"),
        }
    }

    #[test]
    fn parses_serve_subcommand_with_default_host_and_trailing_args() {
        let cli = Cli::try_parse_from([
            "acp-agent",
            "serve",
            "demo-agent",
            "--port",
            "8080",
            "--",
            "--model",
            "gpt-5",
        ])
        .unwrap();

        match cli.command {
            Commands::Serve {
                agent_id,
                port,
                host,
                args,
            } => {
                assert_eq!(agent_id, "demo-agent");
                assert_eq!(port, 8080);
                assert_eq!(host, "127.0.0.1");
                assert_eq!(args, vec!["--model", "gpt-5"]);
            }
            command => panic!("unexpected command: {command:?}"),
        }
    }

    #[test]
    fn parses_serve_subcommand_with_custom_host() {
        let cli = Cli::try_parse_from([
            "acp-agent",
            "serve",
            "demo-agent",
            "--port",
            "9000",
            "--host",
            "0.0.0.0",
        ])
        .unwrap();

        match cli.command {
            Commands::Serve { port, host, .. } => {
                assert_eq!(port, 9000);
                assert_eq!(host, "0.0.0.0");
            }
            command => panic!("unexpected command: {command:?}"),
        }
    }

    #[test]
    fn serve_requires_port() {
        let error = Cli::try_parse_from(["acp-agent", "serve", "demo-agent"]).unwrap_err();

        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[tokio::test]
    async fn execute_install_dispatches_and_prints_outcome() {
        let invocations = Arc::new(Mutex::new(Vec::new()));
        let install_invocations = Arc::clone(&invocations);
        let handlers = CommandHandlers {
            install: Box::new(move |agent_id| {
                let agent_id = agent_id.to_string();
                let invocations = Arc::clone(&install_invocations);
                Box::pin(async move {
                    invocations.lock().unwrap().push(Invocation::Install {
                        agent_id: agent_id.clone(),
                    });
                    Ok(acp_agent::install::InstallOutcome::Binary {
                        agent_id,
                        path: PathBuf::from("/tmp/demo-agent"),
                    })
                })
            }),
            run: Box::new(|_, _| Box::pin(async { panic!("unexpected run invocation") })),
            serve: Box::new(|_, _, _, _| Box::pin(async { panic!("unexpected serve invocation") })),
        };
        let mut output = Vec::new();

        let result = execute_cli(
            Cli {
                command: Commands::Install {
                    agent_id: "demo-agent".to_string(),
                },
            },
            &handlers,
            &mut output,
        )
        .await
        .unwrap();

        assert_eq!(result, CliExit::Success);
        assert_eq!(
            invocations.lock().unwrap().as_slice(),
            &[Invocation::Install {
                agent_id: "demo-agent".to_string(),
            }]
        );
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Installed demo-agent to /tmp/demo-agent\n"
        );
    }

    #[tokio::test]
    async fn execute_install_wraps_errors_with_context() {
        let handlers = CommandHandlers {
            install: Box::new(|_| Box::pin(async { Err(anyhow!("registry unavailable")) })),
            run: Box::new(|_, _| Box::pin(async { panic!("unexpected run invocation") })),
            serve: Box::new(|_, _, _, _| Box::pin(async { panic!("unexpected serve invocation") })),
        };

        let error = execute_cli(
            Cli {
                command: Commands::Install {
                    agent_id: "demo-agent".to_string(),
                },
            },
            &handlers,
            &mut Vec::new(),
        )
        .await
        .unwrap_err();

        assert_eq!(error.to_string(), "failed to install agent \"demo-agent\"");
        assert_eq!(error.root_cause().to_string(), "registry unavailable");
    }

    #[tokio::test]
    async fn execute_run_dispatches_with_args_and_returns_success() {
        let invocations = Arc::new(Mutex::new(Vec::new()));
        let run_invocations = Arc::clone(&invocations);
        let handlers = CommandHandlers {
            install: Box::new(|_| Box::pin(async { panic!("unexpected install invocation") })),
            run: Box::new(move |agent_id, args| {
                let agent_id = agent_id.to_string();
                let args = args.to_vec();
                let invocations = Arc::clone(&run_invocations);
                Box::pin(async move {
                    invocations
                        .lock()
                        .unwrap()
                        .push(Invocation::Run { agent_id, args });
                    Ok(success_exit_status())
                })
            }),
            serve: Box::new(|_, _, _, _| Box::pin(async { panic!("unexpected serve invocation") })),
        };

        let result = execute_cli(
            Cli {
                command: Commands::Run {
                    agent_id: "demo-agent".to_string(),
                    args: vec!["--model".to_string(), "gpt-5".to_string()],
                },
            },
            &handlers,
            &mut Vec::new(),
        )
        .await
        .unwrap();

        assert_eq!(result, CliExit::Success);
        assert_eq!(
            invocations.lock().unwrap().as_slice(),
            &[Invocation::Run {
                agent_id: "demo-agent".to_string(),
                args: vec!["--model".to_string(), "gpt-5".to_string()],
            }]
        );
    }

    #[tokio::test]
    async fn execute_run_returns_non_zero_exit_code() {
        let handlers = CommandHandlers {
            install: Box::new(|_| Box::pin(async { panic!("unexpected install invocation") })),
            run: Box::new(|_, _| Box::pin(async { Ok(exit_status_with_code(17)) })),
            serve: Box::new(|_, _, _, _| Box::pin(async { panic!("unexpected serve invocation") })),
        };

        let result = execute_cli(
            Cli {
                command: Commands::Run {
                    agent_id: "demo-agent".to_string(),
                    args: Vec::new(),
                },
            },
            &handlers,
            &mut Vec::new(),
        )
        .await
        .unwrap();

        assert_eq!(result, CliExit::Code(17));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_run_maps_signal_termination_to_exit_code_one() {
        let handlers = CommandHandlers {
            install: Box::new(|_| Box::pin(async { panic!("unexpected install invocation") })),
            run: Box::new(|_, _| Box::pin(async { Ok(signal_exit_status(9)) })),
            serve: Box::new(|_, _, _, _| Box::pin(async { panic!("unexpected serve invocation") })),
        };

        let result = execute_cli(
            Cli {
                command: Commands::Run {
                    agent_id: "demo-agent".to_string(),
                    args: Vec::new(),
                },
            },
            &handlers,
            &mut Vec::new(),
        )
        .await
        .unwrap();

        assert_eq!(result, CliExit::Code(1));
    }

    #[tokio::test]
    async fn execute_run_wraps_errors_with_context() {
        let handlers = CommandHandlers {
            install: Box::new(|_| Box::pin(async { panic!("unexpected install invocation") })),
            run: Box::new(|_, _| Box::pin(async { Err(anyhow!("spawn failed")) })),
            serve: Box::new(|_, _, _, _| Box::pin(async { panic!("unexpected serve invocation") })),
        };

        let error = execute_cli(
            Cli {
                command: Commands::Run {
                    agent_id: "demo-agent".to_string(),
                    args: Vec::new(),
                },
            },
            &handlers,
            &mut Vec::new(),
        )
        .await
        .unwrap_err();

        assert_eq!(error.to_string(), "failed to run agent \"demo-agent\"");
        assert_eq!(error.root_cause().to_string(), "spawn failed");
    }

    #[tokio::test]
    async fn execute_serve_dispatches_with_host_port_and_args() {
        let invocations = Arc::new(Mutex::new(Vec::new()));
        let serve_invocations = Arc::clone(&invocations);
        let handlers = CommandHandlers {
            install: Box::new(|_| Box::pin(async { panic!("unexpected install invocation") })),
            run: Box::new(|_, _| Box::pin(async { panic!("unexpected run invocation") })),
            serve: Box::new(move |agent_id, host, port, args| {
                let agent_id = agent_id.to_string();
                let host = host.to_string();
                let args = args.to_vec();
                let invocations = Arc::clone(&serve_invocations);
                Box::pin(async move {
                    invocations.lock().unwrap().push(Invocation::Serve {
                        agent_id,
                        host,
                        port,
                        args,
                    });
                    Ok(success_exit_status())
                })
            }),
        };

        let result = execute_cli(
            Cli {
                command: Commands::Serve {
                    agent_id: "demo-agent".to_string(),
                    port: 8080,
                    host: "0.0.0.0".to_string(),
                    args: vec!["--transport".to_string(), "stdio".to_string()],
                },
            },
            &handlers,
            &mut Vec::new(),
        )
        .await
        .unwrap();

        assert_eq!(result, CliExit::Success);
        assert_eq!(
            invocations.lock().unwrap().as_slice(),
            &[Invocation::Serve {
                agent_id: "demo-agent".to_string(),
                host: "0.0.0.0".to_string(),
                port: 8080,
                args: vec!["--transport".to_string(), "stdio".to_string()],
            }]
        );
    }

    #[tokio::test]
    async fn execute_serve_returns_non_zero_exit_code() {
        let handlers = CommandHandlers {
            install: Box::new(|_| Box::pin(async { panic!("unexpected install invocation") })),
            run: Box::new(|_, _| Box::pin(async { panic!("unexpected run invocation") })),
            serve: Box::new(|_, _, _, _| Box::pin(async { Ok(exit_status_with_code(23)) })),
        };

        let result = execute_cli(
            Cli {
                command: Commands::Serve {
                    agent_id: "demo-agent".to_string(),
                    port: 8080,
                    host: "127.0.0.1".to_string(),
                    args: Vec::new(),
                },
            },
            &handlers,
            &mut Vec::new(),
        )
        .await
        .unwrap();

        assert_eq!(result, CliExit::Code(23));
    }

    #[tokio::test]
    async fn execute_serve_wraps_errors_with_context() {
        let handlers = CommandHandlers {
            install: Box::new(|_| Box::pin(async { panic!("unexpected install invocation") })),
            run: Box::new(|_, _| Box::pin(async { panic!("unexpected run invocation") })),
            serve: Box::new(|_, _, _, _| Box::pin(async { Err(anyhow!("bind failed")) })),
        };

        let error = execute_cli(
            Cli {
                command: Commands::Serve {
                    agent_id: "demo-agent".to_string(),
                    port: 8080,
                    host: "127.0.0.1".to_string(),
                    args: Vec::new(),
                },
            },
            &handlers,
            &mut Vec::new(),
        )
        .await
        .unwrap_err();

        assert_eq!(error.to_string(), "failed to serve agent \"demo-agent\"");
        assert_eq!(error.root_cause().to_string(), "bind failed");
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
    fn exit_from_status_returns_one_for_signal_exit() {
        assert_eq!(exit_from_status(signal_exit_status(15)), CliExit::Code(1));
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
