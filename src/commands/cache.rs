//! Local cache management commands: `list --installed`, `uninstall`, and `update`.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::fs;
use tokio::process::Command;

use crate::commands::AgentOutputFormat;
use crate::commands::install::{
    InstallMethod, InstallOutcome, install_from_registry, run_command, run_concurrently,
};
use crate::installer::binary::refresh_binary_target_in;
use crate::installer::cache::{
    CachedAgent, cache_root_dir, list_cached_agents, remove_cached_agent,
    remove_cached_platform_except,
};
use crate::installer::environment::program_available;
use crate::registry::{Platform, Registry, fetch_registry};

/// Prints agents whose binary distributions are present in the local cache.
///
/// This backs the CLI `list --installed` subcommand. The output is a
/// tab-separated table of `id`, `version`, `platform`, and the cache
/// directory that owns the extracted payload, sorted by id/version/platform.
pub async fn list_installed<W: Write>(writer: &mut W) -> Result<()> {
    list_installed_with_format(writer, AgentOutputFormat::Tsv).await
}

/// Prints locally cached binary distributions using `format`.
pub async fn list_installed_with_format<W: Write>(
    writer: &mut W,
    format: AgentOutputFormat,
) -> Result<()> {
    let root_dir = cache_root_dir()?;
    let agents = list_cached_agents(&root_dir).await;
    match format {
        AgentOutputFormat::Tsv => {
            write_installed_list(&agents, writer).context("failed to write installed agent list")
        }
        AgentOutputFormat::Json => write_installed_list_json(&agents, writer)
            .context("failed to write installed agent list as JSON"),
    }
}

fn write_installed_list<W: Write>(agents: &[CachedAgent], writer: &mut W) -> std::io::Result<()> {
    for agent in sorted_cached_agents(agents) {
        writeln!(
            writer,
            "{}\t{}\t{}\t{}",
            agent.agent_id,
            agent.agent_version,
            agent.platform,
            agent.cache_dir.display()
        )?;
    }

    Ok(())
}

fn write_installed_list_json<W: Write>(agents: &[CachedAgent], writer: &mut W) -> Result<()> {
    serde_json::to_writer_pretty(&mut *writer, &sorted_cached_agents(agents))
        .context("failed to serialize installed agent list")?;
    writeln!(writer)?;
    Ok(())
}

fn sorted_cached_agents(agents: &[CachedAgent]) -> Vec<&CachedAgent> {
    let mut agents = agents.iter().collect::<Vec<_>>();
    agents.sort_by(|left, right| {
        left.agent_id
            .cmp(&right.agent_id)
            .then_with(|| left.agent_version.cmp(&right.agent_version))
            .then_with(|| left.platform.cmp(&right.platform))
    });

    agents
}

/// Uninstalls an agent by removing its cached binaries and, for package-manager
/// distributions, its globally installed wrapper.
pub async fn uninstall_agent(agent_id: &str) -> Result<UninstallOutcome> {
    let registry = fetch_registry().await;
    let root_dir = cache_root_dir()?;
    uninstall_from(agent_id, registry, &root_dir).await
}

/// Uninstalls several agents concurrently, returning one result per ID.
pub async fn uninstall_agents(agent_ids: &[String]) -> Vec<(String, Result<UninstallOutcome>)> {
    run_concurrently(agent_ids, |id| async move { uninstall_agent(&id).await }).await
}

async fn uninstall_from(
    agent_id: &str,
    registry: Result<Registry>,
    root_dir: &Path,
) -> Result<UninstallOutcome> {
    let cache_removed = remove_cached_agent(root_dir, agent_id).await?;

    let agent = match registry {
        Ok(registry) => registry.find_agent(agent_id).cloned(),
        Err(error) => {
            if cache_removed {
                eprintln!(
                    "warning: could not reach the registry; removed cached binaries for \
                     \"{agent_id}\" without checking package-manager distributions ({error:#})"
                );
                return Ok(UninstallOutcome::Cache {
                    agent_id: agent_id.to_string(),
                });
            }
            return Err(error).with_context(|| {
                format!("could not determine how agent \"{agent_id}\" was installed")
            });
        }
    };

    if cache_removed {
        return Ok(UninstallOutcome::Cache {
            agent_id: agent_id.to_string(),
        });
    }

    let Some(agent) = agent else {
        bail!("agent \"{agent_id}\" is not installed");
    };

    if let Some(npx) = &agent.distribution.npx {
        return uninstall_npx_package(agent_id, &npx.package).await;
    }

    if let Some(uvx) = &agent.distribution.uvx {
        return uninstall_uvx_package(agent_id, &uvx.package).await;
    }

    bail!("agent \"{agent_id}\" is not installed");
}

async fn uninstall_npx_package(agent_id: &str, package: &str) -> Result<UninstallOutcome> {
    let package = bare_package_name(package);
    let npm_available = program_available("npm")?;
    let npm_installed = if npm_available {
        npm_package_installed(package).await?
    } else {
        false
    };
    let deno_root = deno_install_root()?;
    let deno_installations = find_deno_installations(&deno_root, package).await?;

    if npm_installed && !deno_installations.is_empty() {
        bail!(
            "npm package {package} is installed through both npm and Deno; remove one installation explicitly and retry"
        );
    }

    if npm_installed {
        run_command(
            "npm",
            ["uninstall", "--global", package],
            &format!("npm package {package}"),
        )
        .await?;
        return Ok(UninstallOutcome::PackageManager {
            agent_id: agent_id.to_string(),
            method: InstallMethod::Npm,
            package: package.to_string(),
        });
    }

    if !deno_installations.is_empty() {
        if !program_available("deno")? {
            bail!("npm package {package} is installed through Deno, but deno is not available");
        }
        let mut args = vec!["uninstall".to_string(), "--global".to_string()];
        args.extend(deno_installations);
        run_command("deno", args, &format!("npm package {package}")).await?;
        return Ok(UninstallOutcome::PackageManager {
            agent_id: agent_id.to_string(),
            method: InstallMethod::Deno,
            package: package.to_string(),
        });
    }

    bail!("npm package {package} is not installed through npm or Deno")
}

async fn uninstall_uvx_package(agent_id: &str, package: &str) -> Result<UninstallOutcome> {
    let tool_name = uv_tool_name(package)?;
    run_command(
        "uv",
        ["tool", "uninstall", tool_name],
        &format!("uv package {tool_name}"),
    )
    .await?;

    Ok(UninstallOutcome::PackageManager {
        agent_id: agent_id.to_string(),
        method: InstallMethod::Uvx,
        package: tool_name.to_string(),
    })
}

fn uv_tool_name(package: &str) -> Result<&str> {
    let package = package.trim();
    let end = package
        .char_indices()
        .find_map(|(index, ch)| {
            (ch.is_whitespace() || matches!(ch, '[' | '<' | '>' | '=' | '!' | '~' | '@'))
                .then_some(index)
        })
        .unwrap_or(package.len());
    let name = &package[..end];
    if name.is_empty() {
        bail!("invalid uv package requirement: {package}");
    }
    Ok(name)
}

async fn npm_package_installed(package: &str) -> Result<bool> {
    let output = Command::new("npm")
        .args(["list", "--global", "--depth=0", "--json"])
        .output()
        .await
        .context("failed to inspect globally installed npm packages")?;
    let value: Value = serde_json::from_slice(&output.stdout).with_context(|| {
        let detail = String::from_utf8_lossy(&output.stderr);
        format!(
            "npm did not return a valid global package list: {}",
            detail.trim()
        )
    })?;
    Ok(npm_list_contains(&value, package))
}

fn npm_list_contains(value: &Value, package: &str) -> bool {
    value
        .get("dependencies")
        .and_then(Value::as_object)
        .is_some_and(|dependencies| dependencies.contains_key(package))
}

fn deno_install_root() -> Result<std::path::PathBuf> {
    if let Some(root) = std::env::var_os("DENO_INSTALL_ROOT").filter(|root| !root.is_empty()) {
        return Ok(root.into());
    }
    dirs::home_dir()
        .map(|home| home.join(".deno"))
        .context("could not determine the Deno installation root")
}

async fn find_deno_installations(root_dir: &Path, package: &str) -> Result<Vec<String>> {
    let bin_dir = root_dir.join("bin");
    let mut entries = match fs::read_dir(&bin_dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", bin_dir.display()));
        }
    };
    let mut installations = Vec::new();

    while let Some(entry) = entries.next_entry().await? {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some(name) = file_name.strip_prefix('.').filter(|name| !name.is_empty()) else {
            continue;
        };
        let package_json = entry.path().join("package.json");
        let Ok(bytes) = fs::read(package_json).await else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if npm_list_contains(&value, package) {
            installations.push(name.to_string());
        }
    }

    installations.sort();
    installations.dedup();
    Ok(installations)
}

/// Strips a trailing `@version` specifier from an npm package reference so
/// uninstallers receive a plain package name.
fn bare_package_name(package: &str) -> &str {
    let after_scope = package
        .strip_prefix('@')
        .and_then(|rest| rest.split_once('/').map(|(_, rest)| rest))
        .unwrap_or(package);
    match after_scope.rfind('@') {
        Some(version_separator) => {
            &package[..package.len() - (after_scope.len() - version_separator)]
        }
        None => package,
    }
}

/// Refreshes an agent from the registry.
///
/// A replacement binary is fully prepared before stale cache entries are
/// removed, so a failed refresh leaves the currently working version intact.
pub async fn update_agent(agent_id: &str) -> Result<InstallOutcome> {
    let registry = fetch_registry().await?;
    let root_dir = cache_root_dir()?;
    update_from(agent_id, &registry, &root_dir).await
}

/// Updates several agents concurrently, returning one result per ID.
pub async fn update_agents(agent_ids: &[String]) -> Vec<(String, Result<InstallOutcome>)> {
    run_concurrently(agent_ids, |id| async move { update_agent(&id).await }).await
}

async fn update_from(
    agent_id: &str,
    registry: &Registry,
    root_dir: &Path,
) -> Result<InstallOutcome> {
    let agent = registry.get_agent(agent_id)?;

    if let Some(binary) = &agent.distribution.binary {
        let platform = Platform::current()?;
        if let Some(target) = binary.for_platform(platform) {
            let cached = refresh_binary_target_in(root_dir, agent, platform, target).await?;
            if remove_cached_platform_except(root_dir, agent_id, platform, &cached.cache_dir)
                .await?
            {
                eprintln!("removed stale cached binaries for \"{agent_id}\"");
            }
            return Ok(InstallOutcome::Binary {
                agent_id: agent.id.clone(),
                executable_path: cached.executable_path,
                cache_dir: cached.cache_dir,
            });
        }
    }

    install_from_registry(registry, agent).await
}

/// Outcome data that is printed by the `uninstall` subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UninstallOutcome {
    /// Cached binary distributions were removed from the local cache.
    Cache {
        /// ID of the agent that was uninstalled.
        agent_id: String,
    },
    /// A package manager removed a globally installed wrapper.
    PackageManager {
        /// ID of the agent that was uninstalled.
        agent_id: String,
        /// Which package-manager strategy was used.
        method: InstallMethod,
        /// Package identifier handed to the uninstaller.
        package: String,
    },
}

impl std::fmt::Display for UninstallOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cache { agent_id } => {
                write!(f, "Uninstalled {agent_id} from the local cache")
            }
            Self::PackageManager {
                agent_id,
                method,
                package,
            } => write!(f, "Uninstalled {agent_id} via {method}: {package}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::fs;

    use super::*;
    use crate::installer::cache::{BinaryCacheMetadata, binary_cache_paths};
    use crate::registry::{AgentDistribution, BinaryDistribution, BinaryTarget, RegistryAgent};

    fn cached_agent(
        agent_id: &str,
        version: &str,
        platform: &str,
        cache_dir: &Path,
    ) -> CachedAgent {
        CachedAgent {
            agent_id: agent_id.to_string(),
            agent_version: version.to_string(),
            platform: platform.to_string(),
            cache_dir: cache_dir.to_path_buf(),
            executable_path: cache_dir.join("extracted").join("bin").join(agent_id),
        }
    }

    #[test]
    fn writes_installed_agents_sorted_by_id_version_and_platform() {
        let temp_dir = tempdir().unwrap();
        let agents = vec![
            cached_agent(
                "zebra",
                "0.1.0",
                "linux-x86_64",
                &temp_dir.path().join("zebra-0.1.0"),
            ),
            cached_agent(
                "alpha",
                "1.0.0",
                "linux-x86_64",
                &temp_dir.path().join("alpha-1.0.0-linux"),
            ),
            cached_agent(
                "alpha",
                "1.0.0",
                "darwin-aarch64",
                &temp_dir.path().join("alpha-1.0.0"),
            ),
        ];

        let mut output = Vec::new();
        write_installed_list(&agents, &mut output).unwrap();

        let text = String::from_utf8(output).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("alpha\t1.0.0\tdarwin-aarch64\t"));
        assert!(lines[1].starts_with("alpha\t1.0.0\tlinux-x86_64\t"));
        assert!(lines[2].starts_with("zebra\t0.1.0\tlinux-x86_64\t"));
    }

    #[test]
    fn writes_installed_agents_as_sorted_json_records() {
        let temp_dir = tempdir().unwrap();
        let agents = vec![
            cached_agent(
                "zebra",
                "0.1.0",
                "linux-x86_64",
                &temp_dir.path().join("zebra"),
            ),
            cached_agent(
                "alpha",
                "1.0.0",
                "darwin-aarch64",
                &temp_dir.path().join("alpha"),
            ),
        ];
        let mut output = Vec::new();

        write_installed_list_json(&agents, &mut output).unwrap();

        let value: Value = serde_json::from_slice(&output).unwrap();
        let records = value.as_array().unwrap();
        assert_eq!(records[0]["id"], "alpha");
        assert_eq!(records[0]["version"], "1.0.0");
        assert_eq!(records[1]["id"], "zebra");
        assert!(records[0]["cache_dir"].is_string());
        assert!(records[0]["executable_path"].is_string());
    }

    #[tokio::test]
    async fn uninstall_removes_cached_binaries_even_when_registry_is_unreachable() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        fs::create_dir_all(&paths.cache_dir).await.unwrap();
        let metadata = BinaryCacheMetadata::new(
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            "https://example.com/demo.tar.gz",
            "./demo",
            None,
        );
        fs::write(&paths.metadata_path, serde_json::to_vec(&metadata).unwrap())
            .await
            .unwrap();

        let outcome = uninstall_from("demo", Err(anyhow!("offline")), &cache_root)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            UninstallOutcome::Cache {
                agent_id: "demo".to_string()
            }
        );
        assert!(!paths.cache_dir.exists());
    }

    #[tokio::test]
    async fn uninstall_reports_missing_agent() {
        let temp_dir = tempdir().unwrap();
        let registry = Registry::from_value(json!({
            "version": "1",
            "agents": [
                {
                    "id": "demo",
                    "name": "Demo",
                    "version": "1.0.0",
                    "description": "Demo agent",
                    "authors": ["ACP"],
                    "license": "MIT",
                    "distribution": {
                        "binary": {
                            "linux-x86_64": {
                                "archive": "https://example.com/demo.tar.gz",
                                "cmd": "./bin/demo"
                            }
                        }
                    }
                }
            ]
        }))
        .unwrap();

        let error = uninstall_from("demo", Ok(registry), temp_dir.path())
            .await
            .unwrap_err();

        assert_eq!(error.to_string(), "agent \"demo\" is not installed");
    }

    #[tokio::test]
    async fn update_fails_for_agent_without_distribution() {
        let temp_dir = tempdir().unwrap();
        let registry = Registry {
            version: "1".to_string(),
            agents: vec![RegistryAgent {
                id: "demo".to_string(),
                name: "Demo".to_string(),
                version: "1.0.0".to_string(),
                description: "Demo agent".to_string(),
                repository: None,
                website: None,
                authors: vec!["ACP".to_string()],
                license: "MIT".to_string(),
                icon: None,
                distribution: AgentDistribution {
                    binary: None,
                    npx: None,
                    uvx: None,
                },
            }],
            extensions: None,
        };

        let error = update_from("demo", &registry, temp_dir.path())
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "agent \"demo\" does not have an installable distribution"
        );
    }

    #[test]
    fn strips_version_specifiers_from_package_names() {
        assert_eq!(
            bare_package_name("@agentclientprotocol/codex-acp@1.1.7"),
            "@agentclientprotocol/codex-acp"
        );
        assert_eq!(bare_package_name("acp-demo@2.0.0"), "acp-demo");
        assert_eq!(bare_package_name("@acme/demo"), "@acme/demo");
        assert_eq!(bare_package_name("acp-demo"), "acp-demo");
    }

    #[test]
    fn extracts_uv_tool_names_from_registry_requirements() {
        assert_eq!(
            uv_tool_name("fast-agent-acp==0.9.30").unwrap(),
            "fast-agent-acp"
        );
        assert_eq!(uv_tool_name("minion-code@0.1.44").unwrap(), "minion-code");
        assert_eq!(uv_tool_name("demo[cli]>=1.2").unwrap(), "demo");
        assert!(uv_tool_name("==1.2").is_err());
    }

    #[test]
    fn detects_exact_packages_in_npm_global_list() {
        let list = json!({
            "dependencies": {
                "@acme/demo": { "version": "1.0.0" },
                "demo-extra": { "version": "1.0.0" }
            }
        });

        assert!(npm_list_contains(&list, "@acme/demo"));
        assert!(!npm_list_contains(&list, "demo"));
    }

    #[tokio::test]
    async fn detects_existing_deno_installation_by_package_metadata() {
        let temp_dir = tempdir().unwrap();
        let install_dir = temp_dir.path().join("bin").join(".demo-command");
        fs::create_dir_all(&install_dir).await.unwrap();
        fs::write(
            install_dir.join("package.json"),
            br#"{"dependencies":{"@acme/demo":"1.2.3"}}"#,
        )
        .await
        .unwrap();

        assert_eq!(
            find_deno_installations(temp_dir.path(), "@acme/demo")
                .await
                .unwrap(),
            vec!["demo-command"]
        );
        assert!(
            find_deno_installations(temp_dir.path(), "@acme/other")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn same_version_update_refreshes_but_preserves_cache_on_failure() {
        let temp_dir = tempdir().unwrap();
        let platform = Platform::current().unwrap();
        let old = binary_cache_paths(temp_dir.path(), "demo", "1.0.0", platform);
        write_binary_cache(&old, "demo", "1.0.0", platform, "old").await;
        let unchanged_metadata =
            BinaryCacheMetadata::new("demo", "1.0.0", platform, "not a valid URL", "./demo", None);
        fs::write(
            &old.metadata_path,
            serde_json::to_vec(&unchanged_metadata).unwrap(),
        )
        .await
        .unwrap();
        let registry = binary_registry(
            "demo",
            "1.0.0",
            platform,
            BinaryTarget {
                archive: "not a valid URL".to_string(),
                cmd: "./demo".to_string(),
                sha256: None,
                args: None,
                env: None,
            },
        );

        // A cache hit would return success. This error proves that update
        // attempted a fresh download despite unchanged registry metadata.
        assert!(
            update_from("demo", &registry, temp_dir.path())
                .await
                .is_err()
        );
        assert!(old.cache_dir.exists());
        assert_eq!(
            fs::read(old.extracted_dir.join("demo")).await.unwrap(),
            b"old"
        );
        let install_log = fs::read_to_string(temp_dir.path().join("agent-install.log"))
            .await
            .unwrap();
        assert!(install_log.contains("FAILED agent=demo version=1.0.0"));
    }

    fn binary_registry(
        agent_id: &str,
        version: &str,
        platform: Platform,
        target: BinaryTarget,
    ) -> Registry {
        let mut binary = BinaryDistribution::default();
        match platform {
            Platform::DarwinAarch64 => binary.darwin_aarch64 = Some(target),
            Platform::DarwinX86_64 => binary.darwin_x86_64 = Some(target),
            Platform::LinuxAarch64 => binary.linux_aarch64 = Some(target),
            Platform::LinuxX86_64 => binary.linux_x86_64 = Some(target),
            Platform::WindowsAarch64 => binary.windows_aarch64 = Some(target),
            Platform::WindowsX86_64 => binary.windows_x86_64 = Some(target),
        }
        Registry {
            version: "1".to_string(),
            agents: vec![RegistryAgent {
                id: agent_id.to_string(),
                name: "Demo".to_string(),
                version: version.to_string(),
                description: "Demo agent".to_string(),
                repository: None,
                website: None,
                authors: vec!["ACP".to_string()],
                license: "MIT".to_string(),
                icon: None,
                distribution: AgentDistribution {
                    binary: Some(binary),
                    npx: None,
                    uvx: None,
                },
            }],
            extensions: None,
        }
    }

    async fn write_binary_cache(
        paths: &crate::installer::cache::BinaryCachePaths,
        agent_id: &str,
        version: &str,
        platform: Platform,
        contents: &str,
    ) {
        fs::create_dir_all(&paths.extracted_dir).await.unwrap();
        fs::write(paths.extracted_dir.join("demo"), contents)
            .await
            .unwrap();
        let metadata = BinaryCacheMetadata::new(
            agent_id,
            version,
            platform,
            &format!("https://example.com/demo-{version}.tar.gz"),
            "./demo",
            None,
        );
        fs::write(&paths.metadata_path, serde_json::to_vec(&metadata).unwrap())
            .await
            .unwrap();
    }

    #[test]
    fn displays_uninstall_outcomes() {
        assert_eq!(
            UninstallOutcome::Cache {
                agent_id: "demo".to_string()
            }
            .to_string(),
            "Uninstalled demo from the local cache"
        );
        assert_eq!(
            UninstallOutcome::PackageManager {
                agent_id: "demo".to_string(),
                method: InstallMethod::Npm,
                package: "@acme/demo".to_string(),
            }
            .to_string(),
            "Uninstalled demo via npm: @acme/demo"
        );
        assert_eq!(
            UninstallOutcome::PackageManager {
                agent_id: "demo".to_string(),
                method: InstallMethod::Uvx,
                package: "acme-demo".to_string(),
            }
            .to_string(),
            "Uninstalled demo via uv: acme-demo"
        );
    }
}
