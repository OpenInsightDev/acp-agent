# Registry

The published ACP registry payload is `acp-agent`'s source of agent metadata.
`list` and `search` read it and print agents; `install`, `run`, `serve`, and `server register` resolve agent ids from it.
`list --installed` reads the local cache instead and belongs to the install spec.

Tests never read the published payload, because its contents change between runs.
They serve the checked-in mock catalog at `tests/fixtures/registry.json` from the shared harness.
The catalog is reached through `ACP_AGENT_REGISTRY_URL`, which overrides the published URL; an empty override is ignored, so the default stays the CDN URL.

## Mock catalog

A few agents cover the metadata and distribution shapes the CLI reads.
Archive URLs are inert for `list` and `search`, which never download.

| id | name | covers |
| --- | --- | --- |
| `mock-npx` | `alpha agent` | npm distribution with `args`; lowercase name |
| `mock-binary` | `Beta Agent` | binary distribution with per-platform targets, `args`, `env`, and `sha256` |
| `mock-uvx` | `Beta Agent` | uvx distribution with `env`; name tied with `mock-binary` |
| `mock-bare` | `Gamma Agent` | only required fields, so `repository`, `website`, and `icon` are absent |

Expected `list` order: `mock-npx`, `mock-binary`, `mock-uvx`, `mock-bare`.

## list

`list` prints every agent in the catalog, one per line, as tab-separated `name`, `id`, `description`.
`--json` prints the same records as a JSON array.
Records sort by name compared in lowercase, then by id.
Optional fields that are absent are omitted from the JSON records.

### Tests

- `list::prints_every_agent_as_tsv`: each line has the three columns and the line count matches the catalog size.
- `list::json_records_match_tsv_records`: the two formats carry the same name, id, and description per agent.
- `list::orders_by_name_in_lowercase_then_id`: output follows the expected order above, so a lowercase name sorts before an uppercase one and tied names fall back to id.
- `list::omits_absent_optional_fields`: the `mock-bare` JSON record has no `repository`, `website`, or `icon` key.

## search

`search <query>` prints the agents whose id, name, or description contains the query, matched case-insensitively.
An empty query prints every agent.
Results use the `list` order and formats.
A query that matches nothing prints nothing and still succeeds.

### Tests

- `search::matches_id`: a query equal to an id prints exactly that agent.
- `search::matches_name_and_description`: a query appearing only in a name or a description still prints that agent.
- `search::ignores_case`: an upper-case query matches lower-case catalog text.
- `search::empty_query_prints_every_agent`: an empty query prints the same records as `list`.
- `search::unknown_query_prints_nothing`: a query with no match prints no line and exits zero.
- `search::json_uses_the_same_records_as_tsv`: `--json` carries the same records as the TSV form.

## Loading

A catalog that cannot be fetched, decoded, or validated fails the command with a non-zero exit and a message naming the failure.
An agent without any distribution source is invalid.

### Tests

- `loading::rejects_agent_without_a_distribution_source`: serving `tests/fixtures/registry-invalid.json` makes the command fail with the distribution error.
- `loading::reports_an_unreachable_catalog`: pointing the command at a closed port makes it fail with the fetch error.
