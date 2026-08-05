use std::io::{self, Write};

use anyhow::{Context, Result};

use crate::{
    commands::AgentOutputFormat,
    registry::{Registry, RegistryAgent, fetch_registry},
};

/// Prints registry agents whose name/id/description fuzzily match `query`.
///
/// The output uses the same tab-separated columns as [`super::list::list_agents`],
/// restricted to the matching subset.
pub async fn search_agents<W: Write>(query: &str, writer: &mut W) -> Result<()> {
    search_agents_with_format(query, writer, AgentOutputFormat::Tsv).await
}

/// Prints registry agents matching `query` using `format`.
pub async fn search_agents_with_format<W: Write>(
    query: &str,
    writer: &mut W,
    format: AgentOutputFormat,
) -> Result<()> {
    let registry = fetch_registry().await?;
    match format {
        AgentOutputFormat::Tsv => {
            write_search_results(&registry, query, writer).context("failed to write search results")
        }
        AgentOutputFormat::Json => write_search_results_json(&registry, query, writer)
            .context("failed to write search results as JSON"),
    }
}

fn write_search_results<W: Write>(
    registry: &Registry,
    query: &str,
    writer: &mut W,
) -> io::Result<()> {
    for agent in sorted_matches(registry, query) {
        writeln!(
            writer,
            "{}\t{}\t{}",
            agent.name, agent.id, agent.description
        )?;
    }

    Ok(())
}

fn write_search_results_json<W: Write>(
    registry: &Registry,
    query: &str,
    writer: &mut W,
) -> Result<()> {
    serde_json::to_writer_pretty(&mut *writer, &sorted_matches(registry, query))
        .context("failed to serialize search results")?;
    writeln!(writer)?;
    Ok(())
}

fn sorted_matches<'a>(registry: &'a Registry, query: &str) -> Vec<&'a RegistryAgent> {
    let mut agents = registry.search_agents(query);
    agents.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    agents
}
#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn legacy_search_agents_api_keeps_tsv_signature() {
        fn accepts_legacy_api<W: Write>(query: &str, writer: &mut W) {
            let future = search_agents(query, writer);
            drop(future);
        }

        accepts_legacy_api("agent", &mut Vec::new());
    }

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

    #[test]
    fn writes_full_matching_records_as_json() {
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
                }
            ]
        }))
        .unwrap();

        let mut output = Vec::new();
        write_search_results_json(&registry, "helper", &mut output).unwrap();

        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        let agents = value.as_array().expect("output should be a JSON array");
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["id"], "beta-helper");
        assert_eq!(agents[0]["license"], "MIT");
    }
}
