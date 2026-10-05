use acp_agent::registry::REGISTRY_URL_ENV;

use super::{CliOutput, Command, FixtureServer, Harness};

/// The mock registry catalog served to spawned CLI processes.
///
/// It serves the checked-in fixtures, so assertions can name the exact agents,
/// order, and fields, and no test depends on the live CDN content.
pub struct MockCatalog {
    server: FixtureServer,
}

impl MockCatalog {
    pub async fn start() -> Self {
        Self {
            server: FixtureServer::start(super::repo_fixtures_dir()).await,
        }
    }

    /// URL of the valid mock payload.
    pub fn url(&self) -> String {
        self.server.url("registry.json")
    }

    /// URL of a payload whose agent has no distribution source, so it fails
    /// validation after decoding.
    pub fn invalid_url(&self) -> String {
        self.server.url("registry-invalid.json")
    }

    /// Runs `acp-agent` against this catalog.
    pub async fn run(&self, harness: &Harness, args: &[&str]) -> CliOutput {
        self.command(harness, args).output().await
    }

    /// `acp-agent` invocation aimed at this catalog.
    pub fn command(&self, harness: &Harness, args: &[&str]) -> Command {
        harness.command(args).env(REGISTRY_URL_ENV, self.url())
    }
}
