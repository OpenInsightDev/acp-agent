//! End-to-end tests for [`Registry.md`](Registry.md).
//!
//! Every case runs the CLI against the checked-in mock catalog served by
//! [`MockCatalog`], so the assertions can name exact agents and orders without
//! touching the published payload.
//!
//! | id | name | covers |
//! | --- | --- | --- |
//! | `mock-npx` | `alpha agent` | npm distribution with `args`; lowercase name |
//! | `mock-binary` | `Beta Agent` | binary distribution with per-platform targets, `args`, `env`, and `sha256` |
//! | `mock-uvx` | `Beta Agent` | uvx distribution with `env`; name tied with `mock-binary` |
//! | `mock-bare` | `Gamma Agent` | only required fields, so `repository`, `website`, and `icon` are absent |
//!
//! The catalog is checked in at `tests/fixtures/registry.json`; its archive URLs
//! are inert because `list` and `search` never download.

mod harness;

/// The catalog's agents as `list` prints them: tab-separated `name`, `id`,
/// `description`, in the documented order.
const CATALOG_ROWS: [&str; 4] = [
    "alpha agent\tmock-npx\tnpm-packaged mock agent",
    "Beta Agent\tmock-binary\tbinary-packaged mock agent for every supported platform",
    "Beta Agent\tmock-uvx\tpython-packaged mock agent",
    "Gamma Agent\tmock-bare\tmock agent with only the required fields",
];
const NPX_ROW: &str = CATALOG_ROWS[0];
const BINARY_ROW: &str = CATALOG_ROWS[1];
const UVX_ROW: &str = CATALOG_ROWS[2];
const BARE_ROW: &str = CATALOG_ROWS[3];

mod list {
    use crate::CATALOG_ROWS;
    use crate::harness::{Harness, MockCatalog};

    #[tokio::test]
    async fn tsv() {
        let output = MockCatalog::start()
            .await
            .run(&Harness::new(), &["list"])
            .await;
        output.success();
        assert_eq!(output.lines(), CATALOG_ROWS);
    }

    #[tokio::test]
    async fn json() {
        let output = MockCatalog::start()
            .await
            .run(&Harness::new(), &["list", "--json"])
            .await;
        output.success();
        assert!(
            output.stdout.starts_with("[\n  {"),
            "unexpected array framing:\n{}",
            output.stdout
        );
        assert!(
            output.stdout.ends_with("}\n]\n"),
            "unexpected array framing:\n{}",
            output.stdout
        );

        let records = output.json();
        let records = records.as_array().expect("list --json prints an array");
        assert_eq!(records.len(), CATALOG_ROWS.len());

        // The JSON records are the TSV rows plus the rest of the catalog entry.
        for (record, row) in records.iter().zip(CATALOG_ROWS) {
            let [name, id, description] = row
                .split('\t')
                .collect::<Vec<_>>()
                .try_into()
                .expect("expected three columns");
            assert_eq!(record["name"], name);
            assert_eq!(record["id"], id);
            assert_eq!(record["description"], description);
            for field in ["version", "authors", "license", "distribution"] {
                assert!(record.get(field).is_some(), "{id} is missing {field}");
            }
        }

        let bare = record(records, "mock-bare");
        for field in ["repository", "website", "icon"] {
            assert!(bare.get(field).is_none(), "mock-bare carries {field}");
        }
        let binary = record(records, "mock-binary");
        assert_eq!(binary["repository"], "https://example.invalid/mock-binary");
        assert_eq!(
            binary["website"],
            "https://example.invalid/mock-binary/docs"
        );
        assert_eq!(
            binary["icon"],
            "https://example.invalid/mock-binary/icon.png"
        );
    }

    fn record<'a>(records: &'a [serde_json::Value], id: &str) -> &'a serde_json::Value {
        records
            .iter()
            .find(|record| record["id"] == id)
            .unwrap_or_else(|| panic!("{id} is listed"))
    }
}

mod search {
    use crate::harness::{Harness, MockCatalog};
    use crate::{BARE_ROW, BINARY_ROW, NPX_ROW, UVX_ROW};

    #[tokio::test]
    async fn matches() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        let cases: [(&str, &[&str]); 5] = [
            ("mock-uvx", &[UVX_ROW]),
            ("gamma", &[BARE_ROW]),
            ("python-packaged", &[UVX_ROW]),
            ("MOCK-NPX", &[NPX_ROW]),
            ("  beta  ", &[BINARY_ROW, UVX_ROW]),
        ];
        for (query, expected) in cases {
            let output = catalog.run(&harness, &["search", query]).await;
            output.success();
            assert_eq!(output.lines(), expected, "query {query:?}");
        }
    }

    #[tokio::test]
    async fn empty_query() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        let searched = catalog.run(&harness, &["search", ""]).await;
        searched.success();
        let listed = catalog.run(&harness, &["list"]).await;
        assert_eq!(searched.stdout, listed.success().stdout);
    }

    #[tokio::test]
    async fn no_match() {
        let output = MockCatalog::start()
            .await
            .run(&Harness::new(), &["search", "no-such-agent"])
            .await;
        output.success();
        assert_eq!(output.lines(), Vec::<&str>::new());
    }
}

mod loading {
    use acp_agent::registry::REGISTRY_URL_ENV;

    use crate::harness::{Harness, MockCatalog, free_port};

    #[tokio::test]
    async fn invalid() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        let output = catalog
            .command(&harness, &["list"])
            .env(REGISTRY_URL_ENV, catalog.invalid_url())
            .output()
            .await;
        assert_eq!(output.failure().exit_code(), 1, "\n{}", output.describe());
        assert!(output.stdout.is_empty(), "stdout: {:?}", output.stdout);
        output.stderr_contains("must contain at least one of binary, npx, or uvx");
    }

    #[tokio::test]
    async fn unreachable() {
        let harness = Harness::new();
        let closed = format!("http://127.0.0.1:{}/registry.json", free_port());
        let output = harness
            .command(&["list"])
            .env(REGISTRY_URL_ENV, closed)
            .output()
            .await;
        assert_eq!(output.failure().exit_code(), 1, "\n{}", output.describe());
        assert!(output.stdout.is_empty(), "stdout: {:?}", output.stdout);
        output.stderr_contains("failed to fetch registry payload");
    }
}
