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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Install { agent_id } => {
            let outcome = acp_agent::install::install_agent(&agent_id)
                .await
                .with_context(|| format!("failed to install agent \"{agent_id}\""))?;
            println!("{outcome}");
        }
        Commands::Run { agent_id, args } => {
            let status = acp_agent::run::run_agent(&agent_id, &args)
                .await
                .with_context(|| format!("failed to run agent \"{agent_id}\""))?;

            if let Some(code) = status.code() {
                if code != 0 {
                    std::process::exit(code);
                }
            } else if !status.success() {
                std::process::exit(1);
            }
        }
        Commands::Serve {
            agent_id,
            port,
            host,
            args,
        } => {
            let status = acp_agent::serve::serve_agent(&agent_id, &host, port, &args)
                .await
                .with_context(|| format!("failed to serve agent \"{agent_id}\""))?;

            if let Some(code) = status.code() {
                if code != 0 {
                    std::process::exit(code);
                }
            } else if !status.success() {
                std::process::exit(1);
            }
        }
    }

    Ok(())
}
