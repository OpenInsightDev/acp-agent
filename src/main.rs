use clap::Parser;

use acp_agent::commands::{Cli, CliExit, execute_cli};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();

    match execute_cli(cli, &mut stdout).await? {
        CliExit::Success => Ok(()),
        CliExit::Code(code) => std::process::exit(code),
    }
}
