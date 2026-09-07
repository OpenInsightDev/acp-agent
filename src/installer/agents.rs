//! Agent distribution lifecycle operations.
//!
//! This module owns installation selection, cache refresh, package-manager
//! removal, and binary-cache inventory. Rendering results for a CLI belongs in
//! the command layer.

use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde_json::Value;
use tokio::process::Command;

use crate::installer::binary::{cache_binary_target, refresh_binary_target_in};
use crate::installer::cache::{cache_root_dir, list_cached_agents, remove_cached_agent};
use crate::installer::environment::program_available;
use crate::process;
use crate::registry::{Platform, Registry, RegistryAgent, fetch_registry};
use crate::runner::{PackageRunner, ResolvedDistribution, resolve_distribution};

const OPERATION_CONCURRENCY: usize = 4;

/// Result of installing or updating an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// A binary archive was validated and placed in the local cache.
    Binary {
        /// Registry ID of the agent.
        agent_id: String,
        /// Executable entry point in the cached payload.
        executable_path: PathBuf,
        /// Cache directory that owns the payload.
        cache_dir: PathBuf,
    },
    /// A package manager installed a wrapper for the agent.
    PackageManager {
        /// Registry ID of the agent.
        agent_id: String,
        /// Package runner used to prepare the distribution.
        runner: PackageRunner,
        /// Registry package requirement passed to the installer.
        package: String,
    },
}

/// Result of uninstalling an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UninstallOutcome {
    /// Cached binary distributions were removed.
    Cache {
        /// Registry ID of the agent.
        agent_id: String,
        /// Registry failure that prevented package-manager inspection after
        /// cached binaries were removed.
        registry_error: Option<String>,
    },
    /// A package runner removed a wrapper for the agent.
    PackageManager {
        /// Registry ID of the agent.
        agent_id: String,
        /// Package runner used to remove the distribution.
        runner: PackageRunner,
        /// Package name passed to the uninstaller.
        package: String,
    },
    /// The agent's package distribution is resolved through its runner's own
    /// cache, which the runner manages; there is no launcher or local payload
    /// for `acp-agent` to remove.
    RunnerManaged {
        /// Registry ID of the agent.
        agent_id: String,
        /// Runner that owns the package cache.
        runner: PackageRunner,
    },
}

/// A cached binary distribution found on the local machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstalledAgent {
    /// Registry ID of the cached agent.
    #[serde(rename = "id")]
    pub agent_id: String,
    /// Cached agent version.
    #[serde(rename = "version")]
    pub agent_version: String,
    /// Platform cache key, for example `linux-x86_64`.
    pub platform: String,
    /// Cache directory that owns the extracted payload.
    pub cache_dir: PathBuf,
    /// Executable entry point in the extracted payload.
    pub executable_path: PathBuf,
}

/// Lists cached binary distributions. Package-manager installations are not
/// included because they have no reliable common inventory mechanism.
pub async fn installed_agents() -> Result<Vec<InstalledAgent>> {
    let root_dir = cache_root_dir()?;
    Ok(list_cached_agents(&root_dir)
        .await
        .into_iter()
        .map(|agent| InstalledAgent {
            agent_id: agent.agent_id,
            agent_version: agent.agent_version,
            platform: agent.platform,
            cache_dir: agent.cache_dir,
            executable_path: agent.executable_path,
        })
        .collect())
}

/// Installs an agent using its highest-priority supported distribution.
pub async fn install_agent(agent_id: &str) -> Result<InstallOutcome> {
    let registry = fetch_registry().await?;
    let agent = registry.get_agent(agent_id)?;
    install_from_registry(agent).await
}

/// Installs distinct agent IDs concurrently, preserving first-request order.
pub async fn install_agents(agent_ids: &[String]) -> Vec<(String, Result<InstallOutcome>)> {
    run_concurrently(agent_ids, |id| async move { install_agent(&id).await }).await
}

/// Updates an agent from the latest registry distribution.
///
/// Binary updates prepare a complete immutable digest-keyed entry. Older
/// entries remain available until explicit uninstall or garbage collection so
/// a running server can never lose the executable it resolved before update.
pub async fn update_agent(agent_id: &str) -> Result<InstallOutcome> {
    let registry = fetch_registry().await?;
    let root_dir = cache_root_dir()?;
    update_from(agent_id, &registry, &root_dir).await
}

/// Updates distinct agent IDs concurrently, preserving first-request order.
pub async fn update_agents(agent_ids: &[String]) -> Vec<(String, Result<InstallOutcome>)> {
    run_concurrently(agent_ids, |id| async move { update_agent(&id).await }).await
}

/// Removes cached binaries or a package-manager wrapper for an agent.
pub async fn uninstall_agent(agent_id: &str) -> Result<UninstallOutcome> {
    let registry = fetch_registry().await;
    let root_dir = cache_root_dir()?;
    uninstall_from(agent_id, registry, &root_dir).await
}

/// Uninstalls distinct agent IDs concurrently, preserving first-request order.
pub async fn uninstall_agents(agent_ids: &[String]) -> Vec<(String, Result<UninstallOutcome>)> {
    run_concurrently(agent_ids, |id| async move { uninstall_agent(&id).await }).await
}

/// Installs a registry agent according to binary, npm, then uvx priority.
///
/// Binary distributions are downloaded into the platform cache. Package
/// distributions share one runner decision with `run`/`serve` (see
/// `runner::resolve_distribution`) and are prepared through that runner's own
/// cache — a global npm install, a uv tool install, or a Deno npm-cache
/// warm-up — so the prepared artifacts are exactly what execution consumes,
/// instead of a separate lifecycle that later runs ignore.
pub async fn install_from_registry(agent: &RegistryAgent) -> Result<InstallOutcome> {
    let distribution = resolve_distribution(agent)?.with_context(|| {
        format!(
            "agent \"{}\" does not have an installable distribution",
            agent.id
        )
    })?;
    match distribution {
        ResolvedDistribution::Binary { platform, target } => {
            install_binary(agent, platform, target).await
        }
        ResolvedDistribution::Npm {
            distribution,
            runner,
        } => install_package(agent, runner, &distribution.package).await,
        ResolvedDistribution::Uvx { distribution } => {
            install_package(agent, PackageRunner::Uvx, &distribution.package).await
        }
    }
}

async fn update_from(
    agent_id: &str,
    registry: &Registry,
    root_dir: &Path,
) -> Result<InstallOutcome> {
    let agent = registry.get_agent(agent_id)?;

    let distribution = resolve_distribution(agent)?.with_context(|| {
        format!(
            "agent \"{}\" does not have an installable distribution",
            agent.id
        )
    })?;
    match distribution {
        ResolvedDistribution::Binary { platform, target } => {
            let cached = refresh_binary_target_in(root_dir, agent, platform, target).await?;
            Ok(InstallOutcome::Binary {
                agent_id: agent.id.clone(),
                executable_path: cached.executable_path,
                cache_dir: cached.cache_dir,
            })
        }
        ResolvedDistribution::Npm {
            distribution,
            runner,
        } => install_package(agent, runner, &distribution.package).await,
        ResolvedDistribution::Uvx { distribution } => {
            install_package(agent, PackageRunner::Uvx, &distribution.package).await
        }
    }
}

async fn uninstall_from(
    agent_id: &str,
    registry: Result<Registry>,
    root_dir: &Path,
) -> Result<UninstallOutcome> {
    let cache_removed = remove_cached_agent(root_dir, agent_id).await?;

    let agent = match registry {
        Ok(registry) => registry.find_agent(agent_id).cloned(),
        Err(error) if cache_removed => {
            return Ok(UninstallOutcome::Cache {
                agent_id: agent_id.to_string(),
                registry_error: Some(format!("{error:#}")),
            });
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("could not determine how agent \"{agent_id}\" was installed")
            });
        }
    };

    if cache_removed {
        return Ok(UninstallOutcome::Cache {
            agent_id: agent_id.to_string(),
            registry_error: None,
        });
    }

    let Some(agent) = agent else {
        bail!("agent \"{agent_id}\" is not installed");
    };

    let distribution = resolve_distribution(&agent)?
        .with_context(|| format!("agent \"{agent_id}\" is not installed"))?;
    match distribution {
        ResolvedDistribution::Binary { .. } => bail!("agent \"{agent_id}\" is not installed"),
        ResolvedDistribution::Npm {
            distribution,
            runner,
        } => uninstall_npm_package(agent_id, runner, &distribution.package).await,
        ResolvedDistribution::Uvx { distribution } => {
            uninstall_uvx_package(agent_id, &distribution.package).await
        }
    }
}

async fn install_binary(
    agent: &RegistryAgent,
    platform: Platform,
    target: &crate::registry::BinaryTarget,
) -> Result<InstallOutcome> {
    let cached_binary = cache_binary_target(agent, platform, target).await?;
    Ok(InstallOutcome::Binary {
        agent_id: agent.id.clone(),
        executable_path: cached_binary.executable_path,
        cache_dir: cached_binary.cache_dir,
    })
}

/// Prepares a package distribution through the shared runner's own cache.
///
/// The runner is decided by [`crate::runner::resolve_distribution`], the same
/// function `run`/`serve` use to build the execution command, so preparation
/// and execution can never drift apart:
///
/// - `npm exec` finds globally installed npm launchers, so npm installs the
///   package globally;
/// - `deno x` reads Deno's npm cache, so Deno warms exactly that cache;
/// - `uvx` prefers tools installed with `uv tool install`, so uv installs the
///   tool.
async fn install_package(
    agent: &RegistryAgent,
    runner: PackageRunner,
    package: &str,
) -> Result<InstallOutcome> {
    match runner {
        PackageRunner::Npm => {
            run_command(
                "npm",
                ["install", "--global", package],
                &format!("npm package {package}"),
            )
            .await?;
        }
        PackageRunner::Deno => {
            run_command(
                "deno",
                deno_cache_args(package),
                &format!("npm package {package} via Deno"),
            )
            .await?;
        }
        PackageRunner::Uvx => {
            run_command(
                "uv",
                ["tool", "install", package],
                &format!("uv package {package}"),
            )
            .await?;
        }
    }
    Ok(InstallOutcome::PackageManager {
        agent_id: agent.id.clone(),
        runner,
        package: package.to_string(),
    })
}

async fn uninstall_npm_package(
    agent_id: &str,
    runner: PackageRunner,
    package: &str,
) -> Result<UninstallOutcome> {
    let package = bare_package_name(package);
    if runner == PackageRunner::Npm && program_available("npm")? {
        if npm_package_installed(package).await? {
            run_command(
                "npm",
                ["uninstall", "--global", package],
                &format!("npm package {package}"),
            )
            .await?;
            return Ok(UninstallOutcome::PackageManager {
                agent_id: agent_id.to_string(),
                runner,
                package: package.to_string(),
            });
        }
    }
    // Deno owns its npm cache and does not create a global launcher.
    Ok(UninstallOutcome::RunnerManaged {
        agent_id: agent_id.to_string(),
        runner,
    })
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
        runner: PackageRunner::Uvx,
        package: tool_name.to_string(),
    })
}

async fn run_command<I, S>(program: &str, args: I, subject: &str) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let args = args.into_iter().map(Into::into).collect::<Vec<OsString>>();
    let mut command = Command::new(program);
    command.args(&args);
    let output = process::output(&mut command)
        .await
        .with_context(|| format!("failed to run {program}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let detail = if stderr.is_empty() { stdout } else { stderr };
    if detail.is_empty() {
        bail!("failed to run {program} for {subject}");
    }
    bail!("failed to run {program} for {subject}: {detail}")
}

async fn run_concurrently<T, F, Fut>(agent_ids: &[String], operation: F) -> Vec<(String, Result<T>)>
where
    T: Send + 'static,
    F: Fn(String) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<T>> + Send + 'static,
{
    let mut seen = std::collections::HashSet::with_capacity(agent_ids.len());
    let ids = agent_ids
        .iter()
        .filter(|id| seen.insert((*id).clone()))
        .cloned()
        .collect::<Vec<_>>();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(OPERATION_CONCURRENCY));
    let operation = Arc::new(operation);
    let mut tasks = tokio::task::JoinSet::new();

    for id in &ids {
        let id = id.clone();
        let operation = Arc::clone(&operation);
        let semaphore = Arc::clone(&semaphore);
        tasks.spawn(async move {
            let _permit = semaphore
                .acquire()
                .await
                .expect("semaphore is never closed");
            let task_id = id.clone();
            match tokio::spawn(async move { (task_id.clone(), operation(task_id).await) }).await {
                Ok(result) => result,
                Err(error) => (
                    id.clone(),
                    Err(anyhow!("operation for agent \"{id}\" panicked: {error}")),
                ),
            }
        });
    }

    let mut results = std::collections::HashMap::with_capacity(ids.len());
    while let Some(task) = tasks.join_next().await {
        let (id, result) = task.expect("outer concurrent task does not panic");
        results.insert(id, result);
    }
    ids.into_iter()
        .map(|id| {
            let result = results
                .remove(&id)
                .expect("every spawned task returns a result");
            (id, result)
        })
        .collect()
}

/// Warms Deno's npm cache for an npm package: `deno x` (the shared runner for
/// npm distributions when npm is unavailable) reads exactly this cache, so
/// preparation and execution stay on one lifecycle.
fn deno_cache_args(package: &str) -> [String; 4] {
    [
        "cache".to_string(),
        "--minimum-dependency-age".to_string(),
        "0".to_string(),
        format!("npm:{package}"),
    ]
}

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
    let mut command = Command::new("npm");
    command.args(["list", "--global", "--depth=0", "--json"]);
    let output = process::output(&mut command)
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

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::fs;

    use super::*;
    use crate::installer::cache::{BinaryCacheMetadata, binary_cache_paths};
    use crate::registry::{AgentDistribution, BinaryDistribution, BinaryTarget};

    fn sample_agent() -> RegistryAgent {
        RegistryAgent {
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
        }
    }

    #[tokio::test]
    async fn batch_deduplicates_preserves_order_and_isolates_panics() {
        let ids = vec!["panics".to_string(), "ok".to_string(), "panics".to_string()];
        let results = run_concurrently(&ids, |id| async move {
            if id == "panics" {
                panic!("boom");
            }
            Ok::<_, anyhow::Error>(id)
        })
        .await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "panics");
        assert!(results[0].1.is_err());
        assert!(
            results[0]
                .1
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("boom")
        );
        assert_eq!(results[1].0, "ok");
        assert_eq!(results[1].1.as_deref().unwrap(), "ok");
    }

    #[tokio::test]
    async fn reports_missing_distribution() {
        let error = install_from_registry(&sample_agent()).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "agent \"demo\" does not have an installable distribution"
        );
    }

    #[tokio::test]
    async fn update_reports_missing_distribution() {
        let registry = Registry {
            version: "1".to_string(),
            agents: vec![sample_agent()],
            extensions: None,
        };
        let temp_dir = tempdir().unwrap();

        let error = update_from("demo", &registry, temp_dir.path())
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "agent \"demo\" does not have an installable distribution"
        );
    }

    #[tokio::test]
    async fn uninstall_reports_missing_agent() {
        let registry = Registry {
            version: "1".to_string(),
            agents: vec![sample_agent()],
            extensions: None,
        };
        let temp_dir = tempdir().unwrap();

        let error = uninstall_from("demo", Ok(registry), temp_dir.path())
            .await
            .unwrap_err();

        assert_eq!(error.to_string(), "agent \"demo\" is not installed");
    }

    #[test]
    fn parses_package_requirements() {
        assert_eq!(
            bare_package_name("@agentclientprotocol/codex-acp@1.1.7"),
            "@agentclientprotocol/codex-acp"
        );
        assert_eq!(bare_package_name("acp-demo@2.0.0"), "acp-demo");
        assert_eq!(
            uv_tool_name("fast-agent-acp==0.9.30").unwrap(),
            "fast-agent-acp"
        );
        assert_eq!(uv_tool_name("demo[cli]>=1.2").unwrap(), "demo");
        assert!(uv_tool_name("==1.2").is_err());
        assert_eq!(
            deno_cache_args("@agentclientprotocol/codex-acp@1.1.7"),
            [
                "cache".to_string(),
                "--minimum-dependency-age".to_string(),
                "0".to_string(),
                "npm:@agentclientprotocol/codex-acp@1.1.7".to_string(),
            ]
        );
        assert_eq!(
            deno_cache_args("acp-demo"),
            [
                "cache".to_string(),
                "--minimum-dependency-age".to_string(),
                "0".to_string(),
                "npm:acp-demo".to_string(),
            ]
        );
    }

    #[test]
    fn finds_exact_npm_package() {
        let list = json!({"dependencies": {"@acme/demo": {"version": "1.0.0"}, "demo-extra": {}}});
        assert!(npm_list_contains(&list, "@acme/demo"));
        assert!(!npm_list_contains(&list, "demo"));
    }

    #[tokio::test]
    async fn uninstall_removes_cached_binary_when_registry_is_unavailable() {
        let temp_dir = tempdir().unwrap();
        let root = temp_dir.path().join("cache/acp-agent");
        let paths = binary_cache_paths(&root, "demo", "1.0.0", Platform::LinuxX86_64);
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
        assert_eq!(
            uninstall_from("demo", Err(anyhow!("offline")), &root)
                .await
                .unwrap(),
            UninstallOutcome::Cache {
                agent_id: "demo".to_string(),
                registry_error: Some("offline".to_string())
            }
        );
        assert!(!paths.cache_dir.exists());
    }

    #[tokio::test]
    async fn update_preserves_cached_binary_when_refresh_fails() {
        let temp_dir = tempdir().unwrap();
        let platform = Platform::current().unwrap();
        let old = binary_cache_paths(temp_dir.path(), "demo", "1.0.0", platform);
        fs::create_dir_all(&old.extracted_dir).await.unwrap();
        fs::write(old.extracted_dir.join("demo"), "old")
            .await
            .unwrap();
        let metadata =
            BinaryCacheMetadata::new("demo", "1.0.0", platform, "not a valid URL", "./demo", None);
        fs::write(&old.metadata_path, serde_json::to_vec(&metadata).unwrap())
            .await
            .unwrap();
        let mut binary = BinaryDistribution::default();
        let target = BinaryTarget {
            archive: "not a valid URL".to_string(),
            cmd: "./demo".to_string(),
            sha256: None,
            args: None,
            env: None,
        };
        match platform {
            Platform::DarwinAarch64 => binary.darwin_aarch64 = Some(target),
            Platform::DarwinX86_64 => binary.darwin_x86_64 = Some(target),
            Platform::LinuxAarch64 => binary.linux_aarch64 = Some(target),
            Platform::LinuxX86_64 => binary.linux_x86_64 = Some(target),
        }
        let mut agent = sample_agent();
        agent.distribution.binary = Some(binary);
        let registry = Registry {
            version: "1".to_string(),
            agents: vec![agent],
            extensions: None,
        };
        assert!(
            update_from("demo", &registry, temp_dir.path())
                .await
                .is_err()
        );
        assert_eq!(
            fs::read(old.extracted_dir.join("demo")).await.unwrap(),
            b"old"
        );
        let install_log = fs::read_to_string(temp_dir.path().join("agent-install.log"))
            .await
            .unwrap();
        assert!(install_log.contains("FAILED agent=demo version=1.0.0"));
    }
}
