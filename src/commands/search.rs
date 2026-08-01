use std::io::{self, Write};

use anyhow::{Context, Result};

use crate::registry::{Registry, fetch_registry};

/// Prints registry agents whose name/id/description fuzzily match `query`.
///
/// This mirrors the CLI `search` subcommand that writes the same `name`, `id`,
/// `description` columns as `list`, but only for the matching subset.
pub async fn search_agents<W: Write>(query: &str, writer: &mut W) -> Result<()> {
    let registry = fetch_registry().await?;
    write_search_results(&registry, query, writer).context("failed to write search results")
}

fn write_search_results<W: Write>(
    registry: &Registry,
    query: &str,
    writer: &mut W,
) -> io::Result<()> {
    let mut agents = registry.search_agents(query);
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
    fn writes_fuzzy_search_matches_using_name_id_and_description() {
        let registry = Registry::from_value(json!({
            "version": "1",
            "agents": [
                {
                    "id": "alpha-agent",
                    "name": "Alpha",
                    "version": "1.0.0",
                    "description": "General purpose agent",
                    "authors": ["Example"],
                    "license": "MIT",
                    "distribution": { "npx": { "package": "@acme/alpha" } }
                },
                {
                    "id": "beta-helper",
                    "name": "Beta Helper",
                    "version": "1.0.0",
                    "description": "Useful assistant",
                    "authors": ["Example"],
                    "license": "MIT",
                    "distribution": { "npx": { "package": "@acme/beta" } }
                },
                {
                    "id": "gamma",
                    "name": "Gamma",
                    "version": "1.0.0",
                    "description": "Another tool",
                    "authors": ["Example"],
                    "license": "MIT",
                    "distribution": { "npx": { "package": "@acme/gamma" } }
                }
            ]
        }))
        .unwrap();

        let mut output = Vec::new();
        write_search_results(&registry, "helper", &mut output).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Beta Helper\tbeta-helper\tUseful assistant\n"
        );
    }

    #[test]
    fn writes_empty_output_when_no_agent_matches() {
        let registry = Registry::from_value(json!({
            "version": "1",
            "agents": [
                {
                    "id": "alpha-agent",
                    "name": "Alpha",
                    "version": "1.0.0",
                    "description": "General purpose agent",
                    "authors": ["Example"],
                    "license": "MIT",
                    "distribution": { "npx": { "package": "@acme/alpha" } }
                }
            ]
        }))
        .unwrap();

        let mut output = Vec::new();
        write_search_results(&registry, "missing", &mut output).unwrap();

        assert!(output.is_empty());
    }
}
