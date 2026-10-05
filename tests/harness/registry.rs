use std::fs;

use acp_agent::registry::REGISTRY_URL_ENV;

use super::{
    CliOutput, Command, FixtureServer, Harness, catalog_archive_path, host_platform_key,
    repo_fixtures_dir, sha256_hex,
};

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
            server: FixtureServer::start(repo_fixtures_dir()).await,
        }
    }

    /// Serves the checked-in catalog rendered against `harness`'s fixture
    /// directory: `{fixture_server}` becomes this server's URL, and
    /// `{archive_sha256}` becomes the digest of the archive
    /// [`write_catalog_archives`](super::write_catalog_archives) wrote for the
    /// host platform.
    ///
    /// A test that installs a binary therefore asserts against the exact URL and
    /// digest of bytes it built itself.
    pub async fn rendered(harness: &Harness) -> Self {
        Self::rendered_with(harness, &[]).await
    }

    /// Like [`Self::rendered`], but appends `extra` registry entries to the
    /// catalog's `agents` before rendering.
    ///
    /// A case supplies whole agent objects, so it can declare a distribution mix
    /// the checked-in catalog does not carry — one id on several channels, or a
    /// binary target for a platform other than the host's. Placeholders in an
    /// extra agent are resolved exactly as the checked-in ones are.
    pub async fn rendered_with(harness: &Harness, extra: &[serde_json::Value]) -> Self {
        let server = FixtureServer::start(harness.fixtures_dir()).await;
        copy_repo_fixtures(harness);
        render_registry(harness, &server.base_url(), extra);
        Self { server }
    }

    /// Re-renders the catalog after a test replaced the fixture archive; the
    /// same server keeps serving the directory.
    pub fn rerender(&self, harness: &Harness) {
        render_registry(harness, &self.server.base_url(), &[]);
    }

    /// URL of the valid mock payload.
    pub fn url(&self) -> String {
        self.server.url("registry.json")
    }

    pub fn base_url(&self) -> String {
        self.server.base_url()
    }

    /// How many requests this catalog's server has served for `relative`, so a
    /// test can assert an archive was fetched only once.
    pub fn request_count(&self, relative: &str) -> usize {
        self.server.request_count(relative)
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

/// Copies the checked-in fixtures into the harness fixture directory so an
/// unrendered payload such as `registry-invalid.json` is still reachable.
fn copy_repo_fixtures(harness: &Harness) {
    let fixtures = repo_fixtures_dir();
    for entry in fs::read_dir(&fixtures).expect("the checked-in fixtures are readable") {
        let entry = entry.expect("a checked-in fixture is readable");
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name();
        fs::copy(&path, harness.fixtures_dir().join(name)).unwrap_or_else(|error| {
            panic!(
                "failed to copy {} into the harness: {error}",
                path.display()
            )
        });
    }
}

fn render_registry(harness: &Harness, base_url: &str, extra: &[serde_json::Value]) {
    let template = fs::read_to_string(repo_fixtures_dir().join("registry.json"))
        .expect("the checked-in catalog is readable");
    let mut registry: serde_json::Value =
        serde_json::from_str(&template).expect("the checked-in catalog is JSON");
    let agents = registry["agents"]
        .as_array_mut()
        .expect("the checked-in catalog carries an agents array");
    agents.extend(extra.iter().cloned());

    let archive = harness
        .fixtures_dir()
        .join(catalog_archive_path(host_platform_key()));
    let bytes = fs::read(&archive).unwrap_or_else(|error| {
        panic!(
            "failed to read the host fixture archive {}; call write_catalog_archives first: {error}",
            archive.display()
        )
    });
    let rendered = registry
        .to_string()
        .replace("{fixture_server}", base_url)
        .replace("{archive_sha256}", &sha256_hex(&bytes));
    fs::write(harness.fixtures_dir().join("registry.json"), rendered)
        .expect("failed to write the rendered catalog");
}
