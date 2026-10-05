# Registry

The published ACP registry payload is `acp-agent`'s source of agent metadata.
`list` and `search` read it and print agents; both are specified below.
Commands that act on one agent by id — `install`, `run`, `serve`, and `server register` — resolve that id from the same payload and are specified separately.

The payload URL MUST be `https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json`.
`ACP_AGENT_REGISTRY_URL` overrides it; an empty or blank value is ignored.

## Agent record

Both commands print the same record, one per agent.

The default TSV form MUST be three tab-separated columns in this order, one record per line, with no header line and no quoting, escaping, or padding.
Every line MUST end with a newline, including the last; an empty catalog prints nothing. (`list::tsv`)

| Column | Value |
| --- | --- |
| 1 | `name` |
| 2 | `id` |
| 3 | `description` |

`--json` MUST print one array holding the records, pretty-printed with two-space indentation and followed by a newline. (`list::json`)

| Field | Presence | Value |
| --- | --- | --- |
| `id` | always | Registry id. |
| `name` | always | Display name. |
| `version` | always | Version the catalog publishes for the agent. |
| `description` | always | Short summary. |
| `authors` | always | Array of author names, possibly empty. |
| `license` | always | License declaration. |
| `distribution` | always | Distribution object, copied from the catalog. |
| `repository` | when declared | Source repository URL. |
| `website` | when declared | Documentation site URL. |
| `icon` | when declared | Icon URL. |

An optional field an agent does not declare MUST be omitted from its record, never rendered as `null`. (`list::json`)

## list

`acp-agent list [--json]` MUST print every agent in the catalog and MUST exit `0`.

Records MUST sort by `name` compared in lowercase, then by `id`. (`list::tsv`)
A name that is already lowercase therefore sorts before an uppercase name with the same letters, and two agents sharing a name fall back to their ids.

```text
$ acp-agent list
alpha agent	mock-npx	npm-packaged mock agent
Beta Agent	mock-binary	binary-packaged mock agent for every supported platform
Beta Agent	mock-uvx	python-packaged mock agent
Gamma Agent	mock-bare	mock agent with only the required fields
```

### Tests

- `list::tsv`: one TSV line per catalog agent, with the three columns and the trailing newline.
- `list::json`: the array form, its stream framing, and each record's field presence.

## search

`acp-agent search <query> [--json]` MUST print the agents whose `id`, `name`, or `description` contains `query`, in the record form and the `list` order.

The query MUST be trimmed and then compared as ASCII lowercase against those three fields, so surrounding whitespace is ignored and non-ASCII case pairs do not match. (`search::matches`)
The query MUST NOT be split into words and MUST NOT be interpreted as a pattern: any substring matches.

A query that is empty after trimming MUST print every agent. (`search::empty_query`)
A query that matches nothing MUST print nothing and MUST exit `0`. (`search::no_match`)

```json
$ acp-agent search mock-bare --json
[
  {
    "id": "mock-bare",
    "name": "Gamma Agent",
    "version": "0.0.1",
    "description": "mock agent with only the required fields",
    "authors": [],
    "license": "MIT",
    "distribution": {
      "npx": {
        "package": "@mock/gamma"
      }
    }
  }
]
```

### Tests

- `search::matches`: queries hitting an id, a name, or a description print the agents the rules above select, including upper-case and space-padded queries.
- `search::empty_query`: an empty query prints exactly what `list` prints.
- `search::no_match`: an unmatched query prints no line and exits `0`.

## Loading

A catalog that cannot be fetched, decoded, or validated MUST fail the command with exit code `1`, MUST name the failure on standard error, and MUST leave standard output empty. (`loading::invalid`)
An agent without any distribution source makes a catalog invalid.

```text
$ acp-agent list
Error: failed to list registry agents

Caused by:
    failed to decode registry payload from http://127.0.0.1:8099/registry-invalid.json: agents[0].distribution must contain at least one of binary, npx, or uvx
```

### Tests

- `loading::invalid`: a catalog whose agent declares no distribution exits `1`, names the distribution error on standard error, and prints nothing on standard output.
- `loading::unreachable`: a catalog URL that nothing serves exits `1`, names the fetch error on standard error, and prints nothing on standard output.

## Out of scope

`install`, `run`, `serve`, and `server register` resolve agent ids from the same payload; their behavior belongs to their own docs.
`list --installed` reads the local binary cache instead; see [InstallBinary.md](InstallBinary.md).

## Open questions

Whether `ACP_AGENT_REGISTRY_URL` is a supported public interface or internal test support, and where it is documented.
