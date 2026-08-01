use std::io::{self, Write};

use anyhow::{Context, Result};

use crate::registry::{Registry, fetch_registry};

/// Prints the canonical registry agent directory in a tab-separated table.
///
/// This is what the CLI `list` subcommand prints, and it sorts agents by name
/// (case-insensitive) before outputting `name`, `id`, and `description`.
pub async fn list_agents<W: Write>(writer: &mut W) -> Result<()> {
    let registry = fetch_registry().await?;
    write_agent_list(&registry, writer).context("failed to write agent list")
}

fn write_agent_list<W: Write>(registry: &Registry, writer: &mut W) -> io::Result<()> {
    let mut agents = registry.list_agents().iter().collect::<Vec<_>>();
    agents.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });

    for agent in agents {
        writeln!(
            writer,
            "{}\t{}\t{}",
            agent.name, agent.id, agent.description
        )?;
    }

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
}
