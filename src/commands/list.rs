use std::io::{self, Write};

use anyhow::{Context, Result};

use crate::registry::{Registry, RegistryAgent, fetch_registry};

/// Prints the canonical registry agent directory.
///
/// Defaults to a tab-separated `name`, `id`, `description` table sorted by
/// name (case-insensitive). With `json` set, the full agent records are
/// serialized as a pretty-printed JSON array instead.
pub async fn list_agents<W: Write>(writer: &mut W, json: bool) -> Result<()> {
    let registry = fetch_registry().await?;
    if json {
        write_agent_list_json(&registry, writer).context("failed to write agent list as JSON")
    } else {
        write_agent_list(&registry, writer).context("failed to write agent list")
    }
}

fn sorted_agents(registry: &Registry) -> Vec<&RegistryAgent> {
    let mut agents = registry.list_agents().iter().collect::<Vec<_>>();
    agents.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    agents
}

fn write_agent_list<W: Write>(registry: &Registry, writer: &mut W) -> io::Result<()> {
    for agent in sorted_agents(registry) {
        writeln!(
            writer,
            "{}\t{}\t{}",
            agent.name, agent.id, agent.description
        )?;
    }

    Ok(())
}

fn write_agent_list_json<W: Write>(registry: &Registry, writer: &mut W) -> Result<()> {
    serde_json::to_writer_pretty(&mut *writer, &sorted_agents(registry))
        .context("failed to serialize agent list")?;
    writeln!(writer)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn writes_name_id_and_description_for_each_agent() {
        let registry = Registry::from_value(json!({
            "version": "1",
            "agents": [
                {
                    "id": "z-agent",
                    "name": "Zulu",
                    "version": "1.0.0",
                    "description": "Last agent",
                    "authors": ["Example"],
                    "license": "MIT",
                    "distribution": { "npx": { "package": "@acme/zulu" } }
                },
                {
                    "id": "a-agent",
                    "name": "Alpha",
                    "version": "1.0.0",
                    "description": "First agent",
                    "authors": ["Example"],
                    "license": "MIT",
                    "distribution": { "npx": { "package": "@acme/alpha" } }
                }
            ]
        }))
        .unwrap();

        let mut output = Vec::new();
        write_agent_list(&registry, &mut output).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Alpha\ta-agent\tFirst agent\nZulu\tz-agent\tLast agent\n"
        );
    }

    #[test]
    fn writes_full_agent_records_as_json() {
        let registry = Registry::from_value(json!({
            "version": "1",
            "agents": [
                {
                    "id": "z-agent",
                    "name": "Zulu",
                    "version": "1.0.0",
                    "description": "Last agent",
                    "authors": ["Example"],
                    "license": "MIT",
                    "distribution": { "npx": { "package": "@acme/zulu" } }
                },
                {
                    "id": "a-agent",
                    "name": "Alpha",
                    "version": "1.0.0",
                    "description": "First agent",
                    "authors": ["Example"],
                    "license": "MIT",
                    "distribution": { "npx": { "package": "@acme/alpha" } }
                }
            ]
        }))
        .unwrap();

        let mut output = Vec::new();
        write_agent_list_json(&registry, &mut output).unwrap();

        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        let agents = value.as_array().expect("output should be a JSON array");
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0]["id"], "a-agent");
        assert_eq!(agents[0]["name"], "Alpha");
        assert_eq!(agents[0]["version"], "1.0.0");
        assert_eq!(agents[1]["id"], "z-agent");
        assert_eq!(agents[1]["distribution"]["npx"]["package"], "@acme/zulu");
    }
}
