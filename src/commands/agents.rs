use std::fmt;
use std::io::Write;

use anyhow::{Context, Result};

use super::AgentOutputFormat;
use crate::installer::agents::{InstallMethod, InstallOutcome, InstalledAgent, UninstallOutcome};
use crate::registry::RegistryAgent;

pub(super) fn write_installed_agents<W: Write>(
    writer: &mut W,
    mut agents: Vec<InstalledAgent>,
    format: AgentOutputFormat,
) -> Result<()> {
    agents.sort_by(|left, right| {
        left.agent_id
            .cmp(&right.agent_id)
            .then_with(|| left.agent_version.cmp(&right.agent_version))
            .then_with(|| left.platform.cmp(&right.platform))
    });
    match format {
        AgentOutputFormat::Tsv => {
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
        }
        AgentOutputFormat::Json => {
            serde_json::to_writer_pretty(&mut *writer, &agents)
                .context("failed to serialize installed agent list")?;
            writeln!(writer)?;
        }
    }
    Ok(())
}

pub(super) fn write_registry_agents<W: Write>(
    writer: &mut W,
    mut agents: Vec<&RegistryAgent>,
    format: AgentOutputFormat,
) -> Result<()> {
    agents.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    match format {
        AgentOutputFormat::Tsv => {
            for agent in agents {
                writeln!(
                    writer,
                    "{}\t{}\t{}",
                    agent.name, agent.id, agent.description
                )?;
            }
        }
        AgentOutputFormat::Json => {
            serde_json::to_writer_pretty(&mut *writer, &agents)
                .context("failed to serialize registry agents")?;
            writeln!(writer)?;
        }
    }
    Ok(())
}

pub(super) struct InstallMessage<'a>(pub(super) &'a InstallOutcome);

impl fmt::Display for InstallMessage<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            InstallOutcome::Binary {
                agent_id,
                executable_path,
                cache_dir,
                ..
            } => write!(
                formatter,
                "Installed {agent_id} binary at {} (cache: {})",
                executable_path.display(),
                cache_dir.display()
            ),
            InstallOutcome::PackageManager {
                agent_id,
                method,
                package,
            } => match method {
                InstallMethod::Npm => {
                    write!(formatter, "Installed {agent_id} via npm: {package}")
                }
                InstallMethod::Deno => {
                    write!(formatter, "Prepared {agent_id} via deno cache: {package}")
                }
                InstallMethod::Uvx => {
                    write!(formatter, "Installed {agent_id} via uv: {package}")
                }
            },
        }
    }
}

pub(super) struct UninstallMessage<'a>(pub(super) &'a UninstallOutcome);

impl fmt::Display for UninstallMessage<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            UninstallOutcome::Cache { agent_id, .. } => {
                write!(formatter, "Uninstalled {agent_id} from the local cache")
            }
            UninstallOutcome::PackageManager {
                agent_id,
                method,
                package,
            } => write!(
                formatter,
                "Uninstalled {agent_id} via {}: {package}",
                install_method(*method)
            ),
            UninstallOutcome::RunnerManaged { agent_id, runner } => write!(
                formatter,
                "Nothing to uninstall for {agent_id}: its package is cached by {runner}, which manages its own cache"
            ),
        }
    }
}

fn install_method(method: InstallMethod) -> &'static str {
    match method {
        InstallMethod::Npm => "npm",
        InstallMethod::Deno => "deno",
        InstallMethod::Uvx => "uv",
    }
}

pub(super) fn install_warnings(outcome: &InstallOutcome) -> impl Iterator<Item = String> {
    let warning = match outcome {
        InstallOutcome::Binary {
            agent_id,
            stale_cache_entries_removed: true,
            ..
        } => Some(format!("removed stale cached binaries for \"{agent_id}\"")),
        _ => None,
    };
    warning.into_iter()
}

pub(super) fn uninstall_warnings(outcome: &UninstallOutcome) -> impl Iterator<Item = String> {
    let warning = match outcome {
        UninstallOutcome::Cache {
            agent_id,
            registry_error: Some(error),
        } => Some(format!(
            "warning: could not reach the registry; removed cached binaries for \"{agent_id}\" \
             without checking package-manager distributions ({error})"
        )),
        _ => None,
    };
    warning.into_iter()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::installer::agents::InstalledAgent;
    use crate::registry::Registry;
    use serde_json::json;

    #[test]
    fn renders_registry_agents_in_deterministic_order() {
        let registry = Registry::from_value(json!({
            "version": "1",
            "agents": [
                {"id":"z","name":"Zulu","version":"1","description":"Last","authors":[],"license":"MIT","distribution":{"npx":{"package":"z"}}},
                {"id":"a","name":"alpha","version":"1","description":"First","authors":[],"license":"MIT","distribution":{"npx":{"package":"a"}}}
            ]
        }))
        .unwrap();
        let mut output = Vec::new();
        write_registry_agents(
            &mut output,
            registry.list_agents().iter().collect(),
            AgentOutputFormat::Tsv,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "alpha\ta\tFirst\nZulu\tz\tLast\n"
        );
    }

    #[test]
    fn renders_installed_agents_sorted_in_tsv_and_json() {
        let agents = vec![
            InstalledAgent {
                agent_id: "z".into(),
                agent_version: "2".into(),
                platform: "linux-x86_64".into(),
                cache_dir: "/cache/z".into(),
                executable_path: "/cache/z/bin".into(),
            },
            InstalledAgent {
                agent_id: "a".into(),
                agent_version: "1".into(),
                platform: "darwin-aarch64".into(),
                cache_dir: "/cache/a".into(),
                executable_path: "/cache/a/bin".into(),
            },
        ];

        let mut tsv = Vec::new();
        write_installed_agents(&mut tsv, agents.clone(), AgentOutputFormat::Tsv).unwrap();
        assert_eq!(
            String::from_utf8(tsv).unwrap(),
            "a\t1\tdarwin-aarch64\t/cache/a\nz\t2\tlinux-x86_64\t/cache/z\n"
        );

        let mut json = Vec::new();
        write_installed_agents(&mut json, agents, AgentOutputFormat::Json).unwrap();
        let records: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(records[0]["id"], "a");
        assert_eq!(records[0]["version"], "1");
        assert_eq!(records[0]["platform"], "darwin-aarch64");
        assert_eq!(records[0]["cache_dir"], "/cache/a");
        assert_eq!(records[0]["executable_path"], "/cache/a/bin");
    }

    #[test]
    fn renders_lifecycle_messages_and_warnings() {
        let update = InstallOutcome::Binary {
            agent_id: "demo".into(),
            executable_path: "/cache/demo/bin".into(),
            cache_dir: "/cache/demo".into(),
            stale_cache_entries_removed: true,
        };
        assert_eq!(
            InstallMessage(&update).to_string(),
            "Installed demo binary at /cache/demo/bin (cache: /cache/demo)"
        );
        assert_eq!(
            install_warnings(&update).collect::<Vec<_>>(),
            vec!["removed stale cached binaries for \"demo\""]
        );

        let uninstall = UninstallOutcome::Cache {
            agent_id: "demo".into(),
            registry_error: Some("request timed out".into()),
        };
        assert_eq!(
            UninstallMessage(&uninstall).to_string(),
            "Uninstalled demo from the local cache"
        );
        assert!(
            uninstall_warnings(&uninstall)
                .next()
                .unwrap()
                .contains("without checking package-manager distributions (request timed out)")
        );
    }

    #[test]
    fn renders_package_preparation_and_runner_managed_messages() {
        use crate::installer::agents::InstallMethod;
        use crate::runner::PackageRunner;

        let npm = InstallOutcome::PackageManager {
            agent_id: "demo".into(),
            method: InstallMethod::Npm,
            package: "@acme/demo".into(),
        };
        assert_eq!(
            InstallMessage(&npm).to_string(),
            "Installed demo via npm: @acme/demo"
        );

        // The Deno fallback never creates a launcher: install only warms the
        // npm cache that `deno x` reads, and the message says so.
        let deno = InstallOutcome::PackageManager {
            agent_id: "demo".into(),
            method: InstallMethod::Deno,
            package: "@acme/demo".into(),
        };
        assert_eq!(
            InstallMessage(&deno).to_string(),
            "Prepared demo via deno cache: @acme/demo"
        );

        let uvx = InstallOutcome::PackageManager {
            agent_id: "demo".into(),
            method: InstallMethod::Uvx,
            package: "acme-demo".into(),
        };
        assert_eq!(
            InstallMessage(&uvx).to_string(),
            "Installed demo via uv: acme-demo"
        );

        let managed = UninstallOutcome::RunnerManaged {
            agent_id: "demo".into(),
            runner: PackageRunner::Deno,
        };
        assert_eq!(
            UninstallMessage(&managed).to_string(),
            "Nothing to uninstall for demo: its package is cached by deno, which manages its own cache"
        );
    }
}
