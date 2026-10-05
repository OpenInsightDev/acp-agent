//! End-to-end tests for [`InstallPackage.md`](InstallPackage.md).
//!
//! Every case drives the CLI against the checked-in mock catalog served by the
//! harness, with fake `npm`, `deno`, `uv`, and `uvx` programs on `PATH` that
//! record their argv. Which fakes the harness `bin/` directory holds decides the
//! npm-vs-Deno runner, so the same catalog is exercised both ways. No archive or
//! network is involved.

mod harness;

/// The argv of every recorded invocation, in order.
fn recorded_args(log: &std::path::Path) -> Vec<Vec<String>> {
    harness::fake_invocations(log)
        .into_iter()
        .map(|invocation| invocation.args)
        .collect()
}

mod runner {
    use crate::harness::{Harness, MockCatalog, fake_program};
    use crate::recorded_args;

    #[tokio::test]
    async fn npm() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        let log = harness.temp_dir().join("npm.log");
        fake_program(&harness, "npm", &log);

        catalog
            .run(&harness, &["install", "mock-npx"])
            .await
            .success();
        catalog.run(&harness, &["run", "mock-npx"]).await.success();

        assert_eq!(
            recorded_args(&log),
            vec![
                vec!["install", "--global", "@mock/alpha"],
                vec!["exec", "--", "@mock/alpha", "--stdio"],
            ]
        );
    }

    #[tokio::test]
    async fn deno() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        let log = harness.temp_dir().join("deno.log");
        let deno = fake_program(&harness, "deno", &log);
        // Only the fixture bin directory is on PATH, so no real npm is found.
        let path = deno.parent().expect("fixture scripts live in bin/");

        catalog
            .command(&harness, &["install", "mock-npx"])
            .env("PATH", path)
            .output()
            .await
            .success();
        catalog
            .command(&harness, &["run", "mock-npx"])
            .env("PATH", path)
            .output()
            .await
            .success();

        assert_eq!(
            recorded_args(&log),
            vec![
                vec!["cache", "--minimum-dependency-age", "0", "npm:@mock/alpha"],
                vec![
                    "x",
                    "--allow-all",
                    "--minimum-dependency-age",
                    "0",
                    "@mock/alpha",
                    "--stdio",
                ],
            ]
        );
    }

    #[tokio::test]
    async fn uvx() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        let log = harness.temp_dir().join("uv.log");
        fake_program(&harness, "uv", &log);
        fake_program(&harness, "uvx", &log);

        catalog
            .run(&harness, &["install", "mock-uvx"])
            .await
            .success();
        catalog.run(&harness, &["run", "mock-uvx"]).await.success();

        assert_eq!(
            recorded_args(&log),
            vec![vec!["tool", "install", "mock-beta"], vec!["mock-beta"]]
        );
    }
}

mod install {
    use crate::harness::{Harness, MockCatalog, fake_program};

    #[tokio::test]
    async fn npm() {
        let harness = Harness::new();
        fake_program(&harness, "npm", &harness.temp_dir().join("npm.log"));
        MockCatalog::start()
            .await
            .run(&harness, &["install", "mock-npx"])
            .await
            .success()
            .stdout_contains("Installed mock-npx via npm: @mock/alpha");
    }

    #[tokio::test]
    async fn deno() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        let deno = fake_program(&harness, "deno", &harness.temp_dir().join("deno.log"));
        // Only the fixture bin directory is on PATH, so no real npm is found.
        let path = deno.parent().expect("fixture scripts live in bin/");
        catalog
            .command(&harness, &["install", "mock-npx"])
            .env("PATH", path)
            .output()
            .await
            .success()
            .stdout_contains("Prepared mock-npx via deno cache: @mock/alpha");
    }

    #[tokio::test]
    async fn uvx() {
        let harness = Harness::new();
        fake_program(&harness, "uv", &harness.temp_dir().join("uv.log"));
        MockCatalog::start()
            .await
            .run(&harness, &["install", "mock-uvx"])
            .await
            .success()
            .stdout_contains("Installed mock-uvx via uv: mock-beta");
    }

    #[tokio::test]
    async fn not_in_inventory() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        fake_program(&harness, "npm", &harness.temp_dir().join("npm.log"));

        catalog
            .run(&harness, &["install", "mock-npx"])
            .await
            .success();
        let records = catalog
            .run(&harness, &["list", "--installed", "--json"])
            .await
            .success()
            .json();
        assert_eq!(records, serde_json::json!([]));
    }
}

mod run {
    use crate::harness::{Harness, MockCatalog, fake_invocations, fake_program};
    use crate::recorded_args;

    #[tokio::test]
    async fn args() {
        let harness = Harness::new();
        let log = harness.temp_dir().join("npm.log");
        fake_program(&harness, "npm", &log);

        MockCatalog::start()
            .await
            .run(&harness, &["run", "mock-npx", "--", "--extra"])
            .await
            .success();

        assert_eq!(
            recorded_args(&log),
            [["exec", "--", "@mock/alpha", "--stdio", "--extra"]]
        );
    }

    #[tokio::test]
    async fn env() {
        let harness = Harness::new();
        let log = harness.temp_dir().join("uvx.log");
        fake_program(&harness, "uvx", &log);

        MockCatalog::start()
            .await
            .run(&harness, &["run", "mock-uvx"])
            .await
            .success();

        let invocations = fake_invocations(&log);
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].args, ["mock-beta"]);
        assert_eq!(invocations[0].mock_mode.as_deref(), Some("uvx"));
    }
}

mod uninstall {
    use crate::harness::{FakeInvocation, Harness, MockCatalog, fake_invocations, fake_program};
    use crate::recorded_args;

    #[tokio::test]
    async fn npm() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        let log = harness.temp_dir().join("npm.log");
        fake_program(&harness, "npm", &log);

        catalog
            .command(&harness, &["uninstall", "mock-npx"])
            .env("FAKE_LIST_OUTPUT", r#"{"dependencies":{"@mock/alpha":{}}}"#)
            .output()
            .await
            .success()
            .stdout_contains("Uninstalled mock-npx via npm: @mock/alpha");

        assert_eq!(
            recorded_args(&log),
            vec![
                vec!["list", "--global", "--depth=0", "--json"],
                vec!["uninstall", "--global", "@mock/alpha"],
            ]
        );
    }

    #[tokio::test]
    async fn deno() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        let log = harness.temp_dir().join("deno.log");
        let deno = fake_program(&harness, "deno", &log);
        // Only the fixture bin directory is on PATH, so no real npm is found.
        let path = deno.parent().expect("fixture scripts live in bin/");

        catalog
            .command(&harness, &["uninstall", "mock-npx"])
            .env("PATH", path)
            .output()
            .await
            .success()
            .stdout_contains(
                "Nothing to uninstall for mock-npx: its package is cached by deno, which manages its own cache",
            );

        assert_eq!(fake_invocations(&log), Vec::<FakeInvocation>::new());
    }

    #[tokio::test]
    async fn uvx() {
        let harness = Harness::new();
        let catalog = MockCatalog::start().await;
        let log = harness.temp_dir().join("uv.log");
        fake_program(&harness, "uv", &log);

        catalog
            .run(&harness, &["uninstall", "mock-uvx"])
            .await
            .success()
            .stdout_contains("Uninstalled mock-uvx via uv: mock-beta");

        assert_eq!(recorded_args(&log), [["tool", "uninstall", "mock-beta"]]);
    }
}
