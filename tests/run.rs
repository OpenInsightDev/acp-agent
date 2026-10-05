//! End-to-end tests for [`Run.md`](Run.md).
//!
//! Every case runs the CLI against the checked-in mock catalog served by the
//! harness. The package-channel cases shadow `npm` and `uvx` with the harness
//! fake, which records the argv and the catalog-declared environment each
//! invocation saw. The binary-channel cases write a chosen agent script into the
//! catalog's archive, which is the only way to hand `run` an agent that exits
//! with a code, dies by a signal, or reads its streams, and `MockCatalog::rendered`
//! then resolves the archive URL and digest against exactly those bytes.
//!
//! `MockCatalog::rendered_with` appends extra catalog entries, so a case can
//! exercise a resolution the checked-in catalog does not carry: one id declared
//! on several channels, with or without a host-platform binary target.

mod harness;

use serde_json::{Value, json};

use harness::{
    Harness, MockCatalog, binary_archive, catalog_archive_path, fake_invocations,
    host_platform_key, other_platform_key, write_catalog_archives,
};

/// The checked-in binary agent and the command its archive must contain.
const AGENT_ID: &str = "mock-binary";
const BIN_CMD: &str = "bin/mock-binary";

/// Serves the catalog with a binary archive whose agent runs `script`, so a case
/// can pin the agent's exit code, signal death, and streams.
async fn binary_catalog(harness: &Harness, script: &str) -> MockCatalog {
    let bytes = binary_archive(BIN_CMD, script, &[]);
    write_catalog_archives(harness, &bytes);
    MockCatalog::rendered(harness).await
}

/// A catalog agent declared on a binary target for each of `platforms` plus both
/// package channels, so a case can observe which channel `run` picks.
fn multi_channel_agent(id: &str, npx_package: &str, platforms: &[&str]) -> Value {
    let binary = platforms
        .iter()
        .map(|platform| {
            (
                (*platform).to_string(),
                json!({
                    "archive": format!("{{fixture_server}}/{}", catalog_archive_path(platform)),
                    "cmd": BIN_CMD,
                    "sha256": "{archive_sha256}",
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    json!({
        "id": id,
        "name": "Multi Channel Agent",
        "version": "1.0.0",
        "description": "mock agent declared on a binary and both package channels",
        "authors": [],
        "license": "MIT",
        "distribution": {
            "binary": binary,
            "npx": { "package": npx_package },
            "uvx": { "package": "mock-python-channel" },
        }
    })
}

/// The argv of every recorded invocation, in order.
fn recorded_args(log: &std::path::Path) -> Vec<Vec<String>> {
    fake_invocations(log)
        .into_iter()
        .map(|invocation| invocation.args)
        .collect()
}

mod resolve {
    use crate::harness::{Harness, MockCatalog, fake_invocations, fake_program};
    use crate::{
        AGENT_ID, binary_catalog, catalog_archive_path, host_platform_key, multi_channel_agent,
        other_platform_key,
    };

    #[tokio::test]
    async fn binary() {
        let harness = Harness::new();
        let catalog = binary_catalog(&harness, "#!/bin/sh\nprintf 'binary-ran\\n'\n").await;
        let output = catalog.run(&harness, &["run", AGENT_ID]).await;
        output.success();
        assert_eq!(output.stdout, "binary-ran\n");
        assert_eq!(
            catalog.request_count(&catalog_archive_path(host_platform_key())),
            1,
            "the host archive is fetched once"
        );
        assert!(
            harness.cache_root().join("agents").join(AGENT_ID).is_dir(),
            "the run cached the binary without a prior install"
        );
    }

    #[tokio::test]
    async fn priority() {
        let harness = Harness::new();
        let npm_log = harness.temp_dir().join("npm.log");
        let uvx_log = harness.temp_dir().join("uvx.log");
        fake_program(&harness, "npm", &npm_log);
        fake_program(&harness, "uvx", &uvx_log);
        let bytes =
            crate::binary_archive(crate::BIN_CMD, "#!/bin/sh\nprintf 'binary-ran\\n'\n", &[]);
        crate::write_catalog_archives(&harness, &bytes);
        let catalog = MockCatalog::rendered_with(
            &harness,
            &[multi_channel_agent(
                "mock-multi",
                "@mock/multi",
                &[host_platform_key()],
            )],
        )
        .await;

        let output = catalog.run(&harness, &["run", "mock-multi"]).await;
        output.success();
        assert_eq!(
            output.stdout, "binary-ran\n",
            "the host binary channel wins"
        );
        assert!(
            fake_invocations(&npm_log).is_empty(),
            "npm is never invoked"
        );
        assert!(
            fake_invocations(&uvx_log).is_empty(),
            "uvx is never invoked"
        );
    }

    #[tokio::test]
    async fn fallback() {
        let harness = Harness::new();
        let npm_log = harness.temp_dir().join("npm.log");
        let uvx_log = harness.temp_dir().join("uvx.log");
        fake_program(&harness, "npm", &npm_log);
        fake_program(&harness, "uvx", &uvx_log);
        // Rendering resolves `{archive_sha256}` from the host archive, whether or
        // not the agent under test declares a host binary target.
        let bytes = crate::binary_archive(crate::BIN_CMD, "#!/bin/sh\n", &[]);
        crate::write_catalog_archives(&harness, &bytes);
        let catalog = MockCatalog::rendered_with(
            &harness,
            &[multi_channel_agent(
                "mock-fallback",
                "@mock/fallback",
                &[other_platform_key()],
            )],
        )
        .await;

        let output = catalog.run(&harness, &["run", "mock-fallback"]).await;
        output.success();
        assert_eq!(
            crate::recorded_args(&npm_log),
            [["exec", "--", "@mock/fallback"]],
            "a binary target for another platform leaves npx"
        );
        assert!(fake_invocations(&uvx_log).is_empty(), "npx wins over uvx");
        assert_eq!(
            catalog.request_count(&catalog_archive_path(other_platform_key())),
            0,
            "the other platform's archive is never fetched"
        );
    }

    #[tokio::test]
    async fn unknown() {
        let output = MockCatalog::start()
            .await
            .run(&Harness::new(), &["run", "no-such-agent"])
            .await;
        assert_eq!(output.failure().exit_code(), 1, "\n{}", output.describe());
        assert!(output.stdout.is_empty(), "stdout: {:?}", output.stdout);
        output.stderr_contains("failed to run agent \"no-such-agent\"");
        output.stderr_contains("agent with id \"no-such-agent\" was not found");
    }
}

mod args {
    use crate::harness::{Harness, MockCatalog, fake_invocations, fake_program};
    use crate::recorded_args;

    #[tokio::test]
    async fn catalog_and_user() {
        let harness = Harness::new();
        let log = harness.temp_dir().join("npm.log");
        fake_program(&harness, "npm", &log);

        MockCatalog::start()
            .await
            .run(&harness, &["run", "mock-npx", "--", "--extra", "value"])
            .await
            .success();

        assert_eq!(
            recorded_args(&log),
            [["exec", "--", "@mock/alpha", "--stdio", "--extra", "value"]]
        );
    }

    #[tokio::test]
    async fn hyphen_requires_separator() {
        let output = MockCatalog::start()
            .await
            .run(&Harness::new(), &["run", "mock-npx", "--extra"])
            .await;
        assert_eq!(output.failure().exit_code(), 2, "\n{}", output.describe());
        assert!(output.stdout.is_empty(), "stdout: {:?}", output.stdout);
        output.stderr_contains("unexpected argument '--extra'");
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
        assert_eq!(invocations[0].mock_mode.as_deref(), Some("uvx"));
    }
}

mod exit {
    use crate::harness::Harness;
    use crate::{AGENT_ID, binary_catalog};

    #[tokio::test]
    async fn agent_code() {
        let harness = Harness::new();
        let catalog = binary_catalog(&harness, "#!/bin/sh\nexit 7\n").await;
        let output = catalog.run(&harness, &["run", AGENT_ID]).await;
        assert_eq!(output.failure().exit_code(), 7, "\n{}", output.describe());
    }

    #[tokio::test]
    async fn signal() {
        for (signal, code) in [("TERM", 143), ("KILL", 137)] {
            let harness = Harness::new();
            let script = format!("#!/bin/sh\nkill -{signal} $$\n");
            let catalog = binary_catalog(&harness, &script).await;
            let output = catalog.run(&harness, &["run", AGENT_ID]).await;
            assert_eq!(
                output.exit_code(),
                code,
                "SIG{signal}\n{}",
                output.describe()
            );
        }
    }
}

mod streams {
    use crate::harness::Harness;
    use crate::{AGENT_ID, binary_catalog};

    #[tokio::test]
    async fn inherited() {
        let harness = Harness::new();
        let script = "#!/bin/sh\nread line\necho \"out:$line\"\necho \"err:$line\" >&2\n";
        let catalog = binary_catalog(&harness, script).await;

        let output = catalog
            .command(&harness, &["run", AGENT_ID])
            .stdin_str("ping\n")
            .output()
            .await;

        output.success();
        assert_eq!(output.stdout, "out:ping\n");
        assert_eq!(output.stderr, "err:ping\n");
    }
}
