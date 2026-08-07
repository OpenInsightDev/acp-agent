use std::ffi::OsString;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use tokio::process::Command;

use crate::installer::binary::cache_binary_target;
use crate::installer::environment::program_available;
use crate::registry::{
    BinaryTarget, NpxDistribution, Platform, Registry, RegistryAgent, UvxDistribution,
    fetch_registry,
};

/// Installs an agent by ID using the configured registry distribution.
///
/// The function mirrors the CLI `install` subcommand and returns a
/// descriptive `InstallOutcome` so callers can log where the agent ended up.
pub async fn install_agent(agent_id: &str) -> Result<InstallOutcome> {
    let registry = fetch_registry().await?;
    let agent = registry.get_agent(agent_id)?;

    install_from_registry(&registry, agent).await
}

/// Maximum number of simultaneous agent operations (install/update/uninstall).
const INSTALL_CONCURRENCY: usize = 4;

/// Runs a fallible async operation concurrently over the given agent IDs.
///
/// Each operation is spawned onto its own tokio task so subprocess launches
/// (`npm`/`uv`/`deno`) actually execute in parallel, while a shared semaphore
/// caps how many can run at once. Returns one `(id, result)` pair per input ID
/// in the order the IDs were requested.
pub(crate) async fn run_concurrently<T, F, Fut>(
    agent_ids: &[String],
    operation: F,
) -> Vec<(String, Result<T>)>
where
    T: Send + 'static,
    F: Fn(String) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<T>> + Send + 'static,
{
    let semaphore = Arc::new(tokio::sync::Semaphore::new(INSTALL_CONCURRENCY));
    let operation = Arc::new(operation);
    let mut set = tokio::task::JoinSet::new();

    // Drop duplicate IDs so the same agent cache/package is never operated on
    // concurrently (which would race on shared cache promotion and removal).
    // First-occurrence order is preserved.
    let mut seen = std::collections::HashSet::with_capacity(agent_ids.len());
    let ids = agent_ids
        .iter()
        .filter(|id| seen.insert((*id).clone()))
        .cloned()
        .collect::<Vec<_>>();

    for id in &ids {
        let semaphore = Arc::clone(&semaphore);
        let operation = Arc::clone(&operation);
        let id = id.clone();
        set.spawn(async move {
            let _permit = semaphore
                .acquire()
                .await
                .expect("concurrency semaphore must stay open");
            let task_id = id.clone();
            // Run the operation on a nested task so a panic inside a single
            // agent's operation is surfaced as a `JoinError` instead of
            // unwinding through (and aborting) the rest of the concurrent
            // install/update/uninstall batch.
            let handle = tokio::spawn(async move {
                let result = operation(task_id.clone()).await;
                (task_id, result)
            });
            match handle.await {
                Ok(completed) => completed,
                Err(join_error) => {
                    let message = format!("operation for agent \"{id}\" panicked: {join_error}");
                    (id, Err(anyhow!(message)))
                }
            }
        });
    }

    let mut completed = std::collections::HashMap::with_capacity(ids.len());
    while let Some(joined) = set.join_next().await {
        let (id, result) = joined.expect("outer concurrent task panicked");
        completed.insert(id, result);
    }

    // Re-emit results in the order the IDs were requested so the printed
    // output is stable even though the work completed in parallel.
    ids.into_iter()
        .map(|id| {
            let result = completed
                .remove(&id)
                .expect("every spawned task produced a result");
            (id, result)
        })
        .collect()
}

/// Installs several agents concurrently, returning one result per requested ID.
///
/// The returned vector is in the same order as `agent_ids`; each entry pairs
/// the original ID with its own result so callers can report per-agent success
/// or failure independently.
pub async fn install_agents(agent_ids: &[String]) -> Vec<(String, Result<InstallOutcome>)> {
    run_concurrently(agent_ids, |id| async move { install_agent(&id).await }).await
}

/// Core installer that inspects each distribution in priority order.
///
/// Binary archives are prepared inside the local `acp-agent` cache when a
/// platform-matching release exists; otherwise the function falls back to npm or
/// uv package installers depending on what the registry exposes.
pub async fn install_from_registry(
    _registry: &Registry,
    agent: &RegistryAgent,
) -> Result<InstallOutcome> {
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

async fn install_binary(agent: &RegistryAgent, target: &BinaryTarget) -> Result<InstallOutcome> {
    let platform = Platform::current()?;
    let cached_binary = cache_binary_target(agent, platform, target).await?;

    Ok(InstallOutcome::Binary {
        agent_id: agent.id.clone(),
        executable_path: cached_binary.executable_path,
        cache_dir: cached_binary.cache_dir,
    })
}

pub(crate) async fn run_command<I, S>(program: &str, args: I, subject: &str) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let args_vec: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let output = Command::new(program)
        .args(&args_vec)
        .output()
        .await
        .with_context(|| format!("failed to run {program}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !stderr.is_empty() { stderr } else { stdout };

    if detail.is_empty() {
        bail!("failed to run {program} for {subject}");
    }

    bail!("failed to run {program} for {subject}: {detail}");
}

/// Identifier for how an agent was installed when the CLI reports success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallMethod {
    /// The registry points to an npm package installed through npm.
    Npm,
    /// The registry points to an npm package installed through Deno.
    Deno,
    /// The registry points to a uvx package invoking `uv`.
    Uvx,
}

impl std::fmt::Display for InstallMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let installer = match self {
            Self::Npm => "npm",
            Self::Deno => "deno",
            Self::Uvx => "uv",
        };
        write!(f, "{installer}")
    }
}

/// Outcome data that is printed by the `install` subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// A binary archive was validated and stored in the local cache.
    Binary {
        /// ID of the agent that was installed.
        agent_id: String,
        /// Executable entry point inside the cached distribution.
        executable_path: PathBuf,
        /// Filesystem path of the cache directory.
        cache_dir: PathBuf,
    },
    /// A package manager (npm or uv) installed a wrapper on behalf of the agent.
    PackageManager {
        /// ID of the agent that was installed.
        agent_id: String,
        /// Which package-manager strategy was used.
        method: InstallMethod,
        /// Package identifier handed to the installer.
        package: String,
    },
}

impl std::fmt::Display for InstallOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Binary {
                agent_id,
                executable_path,
                cache_dir,
            } => {
                write!(
                    f,
                    "Installed {agent_id} binary at {} (cache: {})",
                    executable_path.display(),
                    cache_dir.display()
                )
            }
            Self::PackageManager {
                agent_id,
                method,
                package,
            } => write!(f, "Installed {agent_id} via {method}: {package}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::AgentDistribution;

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
    async fn run_concurrently_turns_panicking_operation_into_error() {
        let ids = vec!["panics".to_string(), "ok".to_string()];
        let results = run_concurrently(&ids, |id| async move {
            if id == "panics" {
                panic!("boom");
            }
            Ok::<_, anyhow::Error>(format!("done {id}"))
        })
        .await;

        assert_eq!(results.len(), 2);
        let panicked = results.iter().find(|(id, _)| id == "panics").unwrap();
        assert!(
            panicked.1.is_err(),
            "a panicking operation should surface as an Err, not unwind"
        );
        let error_message = panicked.1.as_ref().unwrap_err().to_string();
        assert!(error_message.contains("panics"));
        assert!(error_message.contains("boom"));
        let ok = results.iter().find(|(id, _)| id == "ok").unwrap();
        assert!(ok.1.is_ok());
    }

    #[tokio::test]
    async fn run_concurrently_deduplicates_agent_ids() {
        let ids = vec!["a".to_string(), "b".to_string(), "a".to_string()];
        let results = run_concurrently(&ids, |id| async move {
            Ok::<_, anyhow::Error>(format!("ran {id}"))
        })
        .await;

        assert_eq!(results.len(), 2);
        assert_eq!(results.iter().filter(|(id, _)| id == "a").count(), 1);
        assert_eq!(results.iter().filter(|(id, _)| id == "b").count(), 1);
    }

    #[tokio::test]
    async fn run_concurrently_preserves_requested_order() {
        let ids = vec!["c".to_string(), "a".to_string(), "b".to_string()];
        let results = run_concurrently(&ids, |id| async move {
            Ok::<_, anyhow::Error>(format!("ran {id}"))
        })
        .await;

        let returned: Vec<_> = results.into_iter().map(|(id, _)| id).collect();
        assert_eq!(
            returned, ids,
            "results should follow the requested ID order"
        );
    }

    #[tokio::test]
    async fn reports_missing_distribution() {
        let agent = sample_agent();

        let error = install_from_registry(
            &Registry {
                version: "1".to_string(),
                agents: vec![],
                extensions: None,
            },
            &agent,
        )
        .await
        .expect_err("install should fail");

        assert_eq!(
            error.to_string(),
            "agent \"demo\" does not have an installable distribution"
        );
    }

    #[test]
    fn displays_binary_cache_outcome() {
        let outcome = InstallOutcome::Binary {
            agent_id: "demo".to_string(),
            executable_path: PathBuf::from("/tmp/acp-agent/demo/bin/demo"),
            cache_dir: PathBuf::from("/tmp/acp-agent/demo"),
        };

        assert_eq!(
            outcome.to_string(),
            "Installed demo binary at /tmp/acp-agent/demo/bin/demo (cache: /tmp/acp-agent/demo)"
        );
    }

    #[test]
    fn uses_current_deno_global_npm_install_syntax() {
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
}
