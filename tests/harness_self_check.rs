//! Self-check for the harness in `tests/harness/`, not a feature/document pair:
//! a broken harness should fail here rather than inside a feature test file.

mod harness;

mod cli_invocation {
    use crate::harness::Harness;

    #[tokio::test]
    async fn help_succeeds_and_reports_usage() {
        let harness = Harness::new();
        harness
            .run(&["--help"])
            .await
            .success()
            .stdout_contains("Usage");
    }

    #[tokio::test]
    async fn missing_arguments_fail_with_a_usage_error() {
        let harness = Harness::new();
        let output = harness.run(&["install"]).await;
        assert_eq!(output.failure().exit_code(), 2, "\n{}", output.describe());
        output.stderr_contains("required arguments");
    }
}

mod installed_inventory {
    use crate::harness::{Harness, host_platform_key, seed_cached_binary};

    #[tokio::test]
    async fn empty_cache_reports_no_installed_agents() {
        let harness = Harness::new();
        let output = harness.run(&["list", "--installed", "--json"]).await;
        assert_eq!(output.success().json(), serde_json::json!([]));
    }

    #[tokio::test]
    async fn seeded_binary_is_listed_from_the_harness_cache() {
        let harness = Harness::new();
        let seeded = seed_cached_binary(&harness, "fixture-agent", "1.2.3", "#!/bin/sh\nexit 0\n");
        let records = harness
            .run(&["list", "--installed", "--json"])
            .await
            .success()
            .json();
        let record = &records[0];
        assert_eq!(record["id"], "fixture-agent");
        assert_eq!(record["version"], "1.2.3");
        assert_eq!(record["platform"], host_platform_key());
        assert_eq!(record["cache_dir"], seeded.cache_dir.display().to_string());
        assert_eq!(
            record["executable_path"],
            seeded.executable_path.display().to_string()
        );
    }

    #[tokio::test]
    async fn cache_entries_stay_inside_the_harness_home() {
        let harness = Harness::new();
        seed_cached_binary(&harness, "fixture-agent", "1.2.3", "#!/bin/sh\nexit 0\n");
        let records = harness
            .run(&["list", "--installed", "--json"])
            .await
            .success()
            .json();
        let cache_dir = records[0]["cache_dir"]
            .as_str()
            .expect("cache_dir is a string");
        assert!(
            cache_dir.starts_with(&harness.cache_root().display().to_string()),
            "{cache_dir} escaped {}",
            harness.cache_root().display()
        );
    }
}

mod named_server_control {
    use std::time::Duration;

    use crate::harness::{Harness, wait_for_http_status};

    #[tokio::test]
    async fn starts_lists_exposes_and_stops_an_instance() {
        let harness = Harness::new();
        let daemon = harness.daemon().await;
        assert_eq!(daemon.socket_path(), harness.socket_path());

        let started = harness.run(&["server", "start", "--port", "0"]).await;
        started
            .success()
            .stdout_contains("started server \"default\" at http://127.0.0.1:");

        let records = harness
            .run(&["server", "list", "--json"])
            .await
            .success()
            .json();
        assert_eq!(records[0]["name"], "default");
        assert_eq!(records[0]["state"], "running");

        let status = harness
            .run(&["server", "status", "--json"])
            .await
            .success()
            .json();
        let address = status["address"].as_str().expect("address is a string");
        assert_eq!(address, records[0]["address"]);
        // No route is registered, so the bound public listener answers 404.
        wait_for_http_status(&format!("{address}/"), 404, Duration::from_secs(5)).await;

        harness.run(&["server", "stop"]).await.success();
        let remaining = harness
            .run(&["server", "list", "--json"])
            .await
            .success()
            .json();
        assert_eq!(remaining, serde_json::json!([]));
    }
}

mod fixture_agent {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    use crate::harness::{AcpAgent, Harness};

    #[test]
    fn echo_agent_answers_initialize_and_echo_with_the_request_id() {
        let harness = Harness::new();
        let script = AcpAgent::Echo.install(&harness, "fixture-echo-agent");
        let mut child = Command::new(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to start the fixture agent");
        let mut stdin = child.stdin.take().expect("fixture agent stdin");
        let stdout = child.stdout.take().expect("fixture agent stdout");
        let mut responses = BufReader::new(stdout).lines();

        let initialize = request(&mut stdin, &mut responses, 7, "initialize");
        assert_eq!(initialize["id"], 7);
        assert_eq!(initialize["result"]["protocolVersion"], 1);

        let echo = request(&mut stdin, &mut responses, 8, "test/echo");
        assert_eq!(echo["id"], 8);
        assert_eq!(echo["result"]["echo"], "ok");

        drop(stdin);
        assert!(child.wait().expect("fixture agent exit").success());
    }

    #[test]
    fn failing_agent_writes_its_stderr_and_exits() {
        let harness = Harness::new();
        let script = AcpAgent::Fail {
            code: 3,
            stderr: "can't run: missing toolchain".to_string(),
        }
        .install(&harness, "fixture-failing-agent");
        let output = Command::new(&script)
            .stdin(Stdio::null())
            .output()
            .expect("failed to start the fixture agent");
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(
            String::from_utf8_lossy(&output.stderr).trim_end(),
            "can't run: missing toolchain"
        );
    }

    fn request(
        stdin: &mut impl Write,
        responses: &mut impl Iterator<Item = std::io::Result<String>>,
        id: u32,
        method: &str,
    ) -> serde_json::Value {
        let request = format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{{}}}}"#);
        stdin
            .write_all(format!("{request}\n").as_bytes())
            .expect("fixture agent request");
        stdin.flush().expect("fixture agent flush");
        let line = responses
            .next()
            .expect("fixture agent closed its output")
            .expect("fixture agent output is not UTF-8");
        serde_json::from_str(&line).expect("fixture agent did not answer with JSON")
    }
}

mod fixture_server {
    use std::time::Duration;

    use crate::harness::{FixtureServer, Harness, http_get, http_post_json, wait_for_http_status};

    #[tokio::test]
    async fn serves_fixture_files_and_rejects_missing_paths() {
        let harness = Harness::new();
        harness.write_fixture_file("registry.json", r#"{"version":"1","agents":[]}"#);
        let server = FixtureServer::start(harness.fixtures_dir()).await;

        let body = http_get(&server.url("registry.json")).await;
        assert_eq!(body.success().json()["version"], "1");
        assert_eq!(body.header("content-type"), Some("application/json"),);

        http_get(&server.url("missing.json")).await.has_status(404);
        http_get(&server.url("archives/missing.tar.gz"))
            .await
            .has_status(404);
    }

    #[tokio::test]
    async fn accepts_json_posts_and_polls_until_a_status_appears() {
        let harness = Harness::new();
        harness.write_fixture_file("acp/response.json", r#"{"echo":"ok"}"#);
        let server = FixtureServer::start(harness.fixtures_dir()).await;

        let posted = http_post_json(&server.url("acp/response.json"), r#"{"id":1}"#).await;
        assert_eq!(posted.success().json()["echo"], "ok");

        let polled = wait_for_http_status(
            &server.url("acp/response.json"),
            200,
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(polled.json()["echo"], "ok");
    }
}

mod binary_archive {
    use flate2::read::GzDecoder;

    use crate::harness::{binary_archive, sha256_hex};

    #[test]
    fn layout_round_trips_through_tar_gz() {
        let archive = binary_archive(
            "bin/fixture-agent",
            "#!/bin/sh\nexit 0\n",
            &[("lib/notice.txt", "fixture distribution")],
        );
        let mut entries = std::collections::BTreeMap::new();
        let mut tar = tar::Archive::new(GzDecoder::new(archive.as_slice()));
        for entry in tar.entries().expect("fixture archive is readable") {
            let mut entry = entry.expect("fixture archive entry is readable");
            let path = entry
                .path()
                .expect("fixture archive path is UTF-8")
                .display()
                .to_string();
            let mut contents = String::new();
            std::io::Read::read_to_string(&mut entry, &mut contents)
                .expect("fixture archive entry is readable");
            entries.insert(path, contents);
        }
        assert_eq!(entries["bin/fixture-agent"], "#!/bin/sh\nexit 0\n");
        assert_eq!(entries["lib/notice.txt"], "fixture distribution");
    }

    #[test]
    fn digest_follows_the_archive_bytes() {
        let archive = binary_archive("bin/fixture-agent", "#!/bin/sh\nexit 0\n", &[]);
        let digest = sha256_hex(&archive);
        assert_eq!(digest.len(), 64);
        assert_ne!(digest, sha256_hex(b"#!/bin/sh\nexit 1\n"),);
    }
}
