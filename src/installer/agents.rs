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
use tokio::fs;
use tokio::process::Command;

use crate::installer::binary::{cache_binary_target, refresh_binary_target_in};
use crate::installer::cache::{
    cache_root_dir, list_cached_agents, remove_cached_agent, remove_cached_platform_except,
};
use crate::installer::environment::program_available;
use crate::registry::{
    BinaryTarget, NpxDistribution, Platform, Registry, RegistryAgent, UvxDistribution,
    fetch_registry,
};

const OPERATION_CONCURRENCY: usize = 4;

/// Package manager that installed or removed an agent wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMethod {
    /// npm installed an npm distribution.
    Npm,
    /// Deno installed an npm distribution.
    Deno,
    /// uv installed a uvx distribution.
    Uvx,
}

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
        /// Whether an update removed older cache entries for this platform.
        stale_cache_entries_removed: bool,
    },
    /// A package manager installed a wrapper for the agent.
    PackageManager {
        /// Registry ID of the agent.
        agent_id: String,
        /// Package-manager strategy used.
        method: InstallMethod,
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
    /// A package manager removed a wrapper for the agent.
    PackageManager {
        /// Registry ID of the agent.
        agent_id: String,
        /// Package-manager strategy used.
        method: InstallMethod,
        /// Package name passed to the uninstaller.
        package: String,
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
/// Binary updates prepare a complete replacement before deleting stale cache
/// entries, so a failed refresh leaves the existing binary usable.
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
pub async fn install_from_registry(agent: &RegistryAgent) -> Result<InstallOutcome> {
    if let Some(binary) = &agent.distribution.binary {
        let platform = Platform::current()?;
        if let Some(target) = binary.for_platform(platform) {
            return install_binary(agent, target).await;
        }
    }

    if let Some(npx) = &agent.distribution.npx {
        return install_npx(agent, npx).await;
    }

    if let Some(uvx) = &agent.distribution.uvx {
        return install_uvx(agent, uvx).await;
    }

    Err(anyhow!(
        "agent \"{}\" does not have an installable distribution",
        agent.id
    ))
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
            let stale_cache_entries_removed =
                remove_cached_platform_except(root_dir, agent_id, platform, &cached.cache_dir)
                    .await?;
            return Ok(InstallOutcome::Binary {
                agent_id: agent.id.clone(),
                executable_path: cached.executable_path,
                cache_dir: cached.cache_dir,
                stale_cache_entries_removed,
            });
        }
    }

    install_from_registry(agent).await
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

    if let Some(npx) = &agent.distribution.npx {
        return uninstall_npx_package(agent_id, &npx.package).await;
    }
    if let Some(uvx) = &agent.distribution.uvx {
        return uninstall_uvx_package(agent_id, &uvx.package).await;
    }
    bail!("agent \"{agent_id}\" is not installed")
}

async fn install_binary(agent: &RegistryAgent, target: &BinaryTarget) -> Result<InstallOutcome> {
    let platform = Platform::current()?;
    let cached_binary = cache_binary_target(agent, platform, target).await?;
    Ok(InstallOutcome::Binary {
        agent_id: agent.id.clone(),
        executable_path: cached_binary.executable_path,
        cache_dir: cached_binary.cache_dir,
        stale_cache_entries_removed: false,
    })
}

async fn install_npx(
    agent: &RegistryAgent,
    distribution: &NpxDistribution,
) -> Result<InstallOutcome> {
    let method = if program_available("npm")? {
        run_command(
            "npm",
            ["install", "--global", distribution.package.as_str()],
            &format!("npm package {}", distribution.package),
        )
        .await?;
        InstallMethod::Npm
    } else {
        run_command(
            "deno",
            deno_install_args(&distribution.package),
            &format!("npm package {} via Deno", distribution.package),
        )
        .await?;
        InstallMethod::Deno
    };
    Ok(InstallOutcome::PackageManager {
        agent_id: agent.id.clone(),
        method,
        package: distribution.package.clone(),
    })
}

async fn install_uvx(
    agent: &RegistryAgent,
    distribution: &UvxDistribution,
) -> Result<InstallOutcome> {
    run_command(
        "uv",
        ["tool", "install", distribution.package.as_str()],
        &format!("uv package {}", distribution.package),
    )
    .await?;
    Ok(InstallOutcome::PackageManager {
        agent_id: agent.id.clone(),
        method: InstallMethod::Uvx,
        package: distribution.package.clone(),
    })
}

async fn uninstall_npx_package(agent_id: &str, package: &str) -> Result<UninstallOutcome> {
    let package = bare_package_name(package);
    let npm_installed = if program_available("npm")? {
        npm_package_installed(package).await?
    } else {
        false
    };
    let deno_installations = find_deno_installations(&deno_install_root()?, package).await?;

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

async fn run_command<I, S>(program: &str, args: I, subject: &str) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let args = args.into_iter().map(Into::into).collect::<Vec<OsString>>();
    let output = Command::new(program)
        .args(&args)
        .output()
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

fn deno_install_args(package: &str) -> [&str; 6] {
    [
        "install",
        "--global",
        "--allow-all",
        "--minimum-dependency-age",
        "0",
        package,
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

fn deno_install_root() -> Result<PathBuf> {
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
        let Ok(bytes) = fs::read(entry.path().join("package.json")).await else {
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

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::fs;

    use super::*;
    use crate::installer::cache::{BinaryCacheMetadata, binary_cache_paths};
    use crate::registry::{AgentDistribution, BinaryDistribution};

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
            deno_install_args("@agentclientprotocol/codex-acp@1.1.7"),
            [
                "install",
                "--global",
                "--allow-all",
                "--minimum-dependency-age",
                "0",
                "@agentclientprotocol/codex-acp@1.1.7",
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
    async fn detects_deno_installation_by_package_metadata() {
        let temp_dir = tempdir().unwrap();
        let installation = temp_dir.path().join("bin/.demo-command");
        fs::create_dir_all(&installation).await.unwrap();
        fs::write(
            installation.join("package.json"),
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
            Platform::WindowsAarch64 => binary.windows_aarch64 = Some(target),
            Platform::WindowsX86_64 => binary.windows_x86_64 = Some(target),
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
