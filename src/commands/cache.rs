//! Local cache management commands: `list --installed`, `uninstall`, and `update`.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::commands::install::{InstallMethod, InstallOutcome, install_from_registry, run_command};
use crate::installer::cache::{
    CachedAgent, cache_root_dir, list_cached_agents, remove_cached_agent, remove_cached_platform,
};
use crate::installer::environment::program_available;
use crate::registry::{Platform, Registry, fetch_registry};

/// Prints agents whose binary distributions are present in the local cache.
///
/// This backs the CLI `list --installed` subcommand. The output is a
/// tab-separated table of `id`, `version`, `platform`, and the cache
/// directory that owns the extracted payload, sorted by id/version/platform.
pub async fn list_installed<W: Write>(writer: &mut W) -> Result<()> {
    let root_dir = cache_root_dir()?;
    let agents = list_cached_agents(&root_dir).await;
    write_installed_list(&agents, writer).context("failed to write installed agent list")
}

fn write_installed_list<W: Write>(agents: &[CachedAgent], writer: &mut W) -> std::io::Result<()> {
    let mut agents = agents.to_vec();
    agents.sort_by(|left, right| {
        left.agent_id
            .cmp(&right.agent_id)
            .then_with(|| left.agent_version.cmp(&right.agent_version))
            .then_with(|| left.platform.cmp(&right.platform))
    });

    for agent in agents {
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

/// Uninstalls an agent by removing its cached binaries and, for package-manager
/// distributions, its globally installed wrapper.
pub async fn uninstall_agent(agent_id: &str) -> Result<UninstallOutcome> {
    let registry = fetch_registry().await;
    let root_dir = cache_root_dir()?;
    uninstall_from(agent_id, registry, &root_dir).await
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
    if program_available("npm")? {
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

    if program_available("deno")? {
        run_command(
            "deno",
            ["uninstall", "--global", package],
            &format!("npm package {package}"),
        )
        .await?;
        return Ok(UninstallOutcome::PackageManager {
            agent_id: agent_id.to_string(),
            method: InstallMethod::Deno,
            package: package.to_string(),
        });
    }

    bail!("cannot uninstall npm package {package}: neither npm nor deno is available")
}

async fn uninstall_uvx_package(agent_id: &str, package: &str) -> Result<UninstallOutcome> {
    run_command(
        "uv",
        ["tool", "uninstall", package],
        &format!("uv package {package}"),
    )
    .await?;

    Ok(UninstallOutcome::PackageManager {
        agent_id: agent_id.to_string(),
        method: InstallMethod::Uvx,
        package: package.to_string(),
    })
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
/// Cached binary distributions for the current platform are discarded before
/// the registry's preferred distribution is (re)installed, so the agent is
/// brought to the latest published version.
pub async fn update_agent(agent_id: &str) -> Result<InstallOutcome> {
    let registry = fetch_registry().await?;
    let root_dir = cache_root_dir()?;
    update_from(agent_id, &registry, &root_dir).await
}

async fn update_from(
    agent_id: &str,
    registry: &Registry,
    root_dir: &Path,
) -> Result<InstallOutcome> {
    let agent = registry.get_agent(agent_id)?;

    if let Some(binary) = &agent.distribution.binary {
        let platform = Platform::current()?;
        if binary.for_platform(platform).is_some()
            && remove_cached_platform(root_dir, agent_id, platform).await?
        {
            eprintln!("removed stale cached binaries for \"{agent_id}\"");
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
    use crate::installer::cache::binary_cache_paths;
    use crate::registry::{AgentDistribution, RegistryAgent};

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

    #[tokio::test]
    async fn uninstall_removes_cached_binaries_even_when_registry_is_unreachable() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        fs::create_dir_all(&paths.cache_dir).await.unwrap();

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
