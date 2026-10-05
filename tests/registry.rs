//! End-to-end tests for [`Registry.md`](Registry.md).

mod harness;

mod list {
    use crate::harness::{Harness, MockCatalog};

    const EXPECTED_TSV: [&str; 4] = [
        "alpha agent\tmock-npx\tnpm-packaged mock agent",
        "Beta Agent\tmock-binary\tbinary-packaged mock agent for every supported platform",
        "Beta Agent\tmock-uvx\tpython-packaged mock agent",
        "Gamma Agent\tmock-bare\tmock agent with only the required fields",
    ];

    #[tokio::test]
    async fn prints_every_agent_as_tsv() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        let output = catalog.run(&harness, &["list"]).await;
        output.success();
        assert_eq!(output.lines(), EXPECTED_TSV);
    }

    #[tokio::test]
    async fn json_records_match_tsv_records() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        let records = catalog
            .run(&harness, &["list", "--json"])
            .await
            .success()
            .json();
        let records = records.as_array().expect("list --json prints an array");
        assert_eq!(records.len(), EXPECTED_TSV.len());
        for (record, line) in records.iter().zip(EXPECTED_TSV) {
            let [name, id, description] = line
                .split('\t')
                .collect::<Vec<_>>()
                .try_into()
                .expect("expected three columns");
            assert_eq!(record["name"], name);
            assert_eq!(record["id"], id);
            assert_eq!(record["description"], description);
        }
    }

    #[tokio::test]
    async fn orders_by_name_in_lowercase_then_id() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        let records = catalog
            .run(&harness, &["list", "--json"])
            .await
            .success()
            .json();
        let ids: Vec<&str> = records
            .as_array()
            .expect("list --json prints an array")
            .iter()
            .map(|record| record["id"].as_str().expect("id is a string"))
            .collect();
        assert_eq!(ids, ["mock-npx", "mock-binary", "mock-uvx", "mock-bare"]);
    }

    #[tokio::test]
    async fn omits_absent_optional_fields() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        let records = catalog
            .run(&harness, &["list", "--json"])
            .await
            .success()
            .json();
        let records = records.as_array().expect("list --json prints an array");
        let bare = records
            .iter()
            .find(|record| record["id"] == "mock-bare")
            .expect("mock-bare is listed");
        for field in ["repository", "website", "icon"] {
            assert!(bare.get(field).is_none(), "mock-bare carries {field}");
        }
        let binary = records
            .iter()
            .find(|record| record["id"] == "mock-binary")
            .expect("mock-binary is listed");
        assert_eq!(binary["repository"], "https://example.invalid/mock-binary");
        assert_eq!(
            binary["website"],
            "https://example.invalid/mock-binary/docs"
        );
    }
}

mod search {
    use crate::harness::{Harness, MockCatalog};

    const MOCK_UVX_TSV: &str = "Beta Agent\tmock-uvx\tpython-packaged mock agent";
    const MOCK_BARE_TSV: &str = "Gamma Agent\tmock-bare\tmock agent with only the required fields";

    #[tokio::test]
    async fn matches_id() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        let output = catalog.run(&harness, &["search", "mock-uvx"]).await;
        output.success();
        assert_eq!(output.lines(), [MOCK_UVX_TSV]);
    }

    #[tokio::test]
    async fn matches_name_and_description() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        let by_name = catalog.run(&harness, &["search", "gamma"]).await;
        by_name.success();
        assert_eq!(by_name.lines(), [MOCK_BARE_TSV]);

        let by_description = catalog.run(&harness, &["search", "python-packaged"]).await;
        by_description.success();
        assert_eq!(by_description.lines(), [MOCK_UVX_TSV]);
    }

    #[tokio::test]
    async fn ignores_case() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        let output = catalog.run(&harness, &["search", "MOCK-NPX"]).await;
        output.success();
        assert_eq!(
            output.lines(),
            ["alpha agent\tmock-npx\tnpm-packaged mock agent"]
        );
    }

    #[tokio::test]
    async fn empty_query_prints_every_agent() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        let searched = catalog.run(&harness, &["search", ""]).await;
        searched.success();
        let listed = catalog
            .run(&harness, &["list"])
            .await
            .success()
            .stdout
            .clone();
        assert_eq!(searched.stdout, listed);
    }

    #[tokio::test]
    async fn unknown_query_prints_nothing() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        let output = catalog.run(&harness, &["search", "no-such-agent"]).await;
        output.success();
        assert_eq!(output.lines(), Vec::<&str>::new());
    }

    #[tokio::test]
    async fn json_uses_the_same_records_as_tsv() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        let as_json = catalog.run(&harness, &["search", "beta", "--json"]).await;
        let records = as_json.success().json();
        let ids: Vec<&str> = records
            .as_array()
            .expect("search --json prints an array")
            .iter()
            .map(|record| record["id"].as_str().expect("id is a string"))
            .collect();
        assert_eq!(ids, ["mock-binary", "mock-uvx"]);

        let as_tsv = catalog.run(&harness, &["search", "beta"]).await;
        as_tsv.success();
        let tsv_ids: Vec<&str> = as_tsv
            .lines()
            .iter()
            .map(|line| line.split('\t').nth(1).expect("expected an id column"))
            .collect();
        assert_eq!(tsv_ids, ids);
    }
}

mod loading {
    use acp_agent::registry::REGISTRY_URL_ENV;

    use crate::harness::{Harness, MockCatalog, free_port};

    #[tokio::test]
    async fn rejects_agent_without_a_distribution_source() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        let output = catalog
            .command(&harness, &["list"])
            .env(REGISTRY_URL_ENV, catalog.invalid_url())
            .output()
            .await;
        output
            .failure()
            .stderr_contains("must contain at least one of binary, npx, or uvx");
    }

    #[tokio::test]
    async fn reports_an_unreachable_catalog() {
        let harness = Harness::new();
        let closed = format!("http://127.0.0.1:{}/registry.json", free_port());
        let output = harness
            .command(&["list"])
            .env(REGISTRY_URL_ENV, closed)
            .output()
            .await;
        output
            .failure()
            .stderr_contains("failed to fetch registry payload");
    }
}
