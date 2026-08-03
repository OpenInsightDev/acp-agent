use clap::Parser;

use acp_agent::commands::{Cli, CliExit, execute_cli};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();

    match execute_cli(cli, &mut stdout).await? {
        CliExit::Success => Ok(()),
        CliExit::Code(code) => std::process::exit(code),
    }
}

/// Routes `tracing` events from the ACP HTTP server to this process's stderr.
///
/// Without a subscriber, the library's agent-launch failures (which include
/// the agent stderr tail) would be swallowed and invisible in `docker logs`.
/// Stderr keeps logs on the same stream as the CLI's own diagnostics; the
/// default `info` level shows connection lifecycle, `RUST_LOG` overrides.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
