//! End-to-end tests for [`Serve.md`](Serve.md).
//!
//! Every case runs the built `acp-agent serve` binary against the checked-in
//! mock catalog: `MockCatalog::rendered` resolves the catalog's binary archives
//! to a runnable script the harness built itself, and `serve` installs that
//! binary on demand through the fixture server. The cases therefore observe the
//! process boundary — the address it prints on standard error, the HTTP and
//! WebSocket responses, and the exit status — rather than the crate's internals.
//!
//! `AcpAgent::Echo` is the served agent that answers `initialize` and
//! `test/echo`; `AcpAgent::Fail` stands in for a launch that fails so the
//! readiness failure path is reachable. The harness owns the `serve` process
//! (see [`Serve`]) and the WebSocket client (see [`WsClient`]).

mod harness;

use std::time::Duration;

use reqwest::header::CONTENT_TYPE;

use harness::{CliOutput, Harness, MockCatalog, Serve, binary_archive, write_catalog_archives};

/// Registry id the mock catalog publishes as a binary agent.
const AGENT_ID: &str = "mock-binary";
/// Executable path inside the catalog's binary archive.
const CMD: &str = "bin/mock-binary";
/// Header carrying an ACP connection id.
const CONNECTION_ID: &str = "acp-connection-id";

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}"#;
const ECHO: &str = r#"{"jsonrpc":"2.0","id":2,"method":"test/echo","params":{}}"#;

/// Renders the mock catalog so `mock-binary` resolves to a binary whose
/// executable is `script`.
async fn catalog_with(harness: &Harness, script: &str) -> MockCatalog {
    write_catalog_archives(harness, &binary_archive(CMD, script, &[]));
    MockCatalog::rendered(harness).await
}

/// Starts `acp-agent serve mock-binary <args>` against `catalog`.
async fn serve(harness: &Harness, catalog: &MockCatalog, args: &[&str]) -> Serve {
    let mut command = vec!["serve", AGENT_ID];
    command.extend_from_slice(args);
    Serve::start(harness, catalog.command(harness, &command)).await
}

/// Runs `acp-agent serve mock-binary <args>` to completion against `catalog`.
async fn serve_output(harness: &Harness, catalog: &MockCatalog, args: &[&str]) -> CliOutput {
    let mut command = vec!["serve", AGENT_ID];
    command.extend_from_slice(args);
    catalog.command(harness, &command).output().await
}

/// Sends a request built by the caller and fails the case on a timeout.
async fn send(request: reqwest::RequestBuilder) -> reqwest::Response {
    tokio::time::timeout(Duration::from_secs(10), request.send())
        .await
        .expect("the HTTP request timed out")
        .expect("the HTTP request failed")
}

/// Opens one connection on `endpoint` and returns its connection id.
async fn initialize(client: &reqwest::Client, endpoint: &str) -> String {
    let response = send(
        client
            .post(endpoint)
            .header(CONTENT_TYPE, "application/json")
            .body(INITIALIZE),
    )
    .await;
    assert_eq!(response.status(), 200);
    response
        .headers()
        .get(CONNECTION_ID)
        .expect("initialize returns a connection id")
        .to_str()
        .expect("the connection id is text")
        .to_string()
}

mod startup {
    use crate::harness::{AcpAgent, Harness, free_port, http_get};
    use crate::{catalog_with, serve};

    #[tokio::test]
    async fn address_lines() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let serve = serve(&harness, &catalog, &[]).await;

        assert!(
            serve.stdout_text().is_empty(),
            "standard output stays empty: {:?}",
            serve.stdout_text()
        );
        assert_eq!(
            serve.stderr_text(),
            format!(
                "Serving ACP agent at {base}/acp (WebSocket available on the same endpoint)\nAgent readiness probe at {base}/readyz\n",
                base = serve.base_url()
            )
        );
        assert!(
            serve.base_url().starts_with("http://127.0.0.1:"),
            "the default host is loopback: {}",
            serve.base_url()
        );
        http_get(&serve.url("/health")).await.has_status(200);
    }

    #[tokio::test]
    async fn requested_port() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let port = free_port().to_string();
        let serve = serve(&harness, &catalog, &["--port", &port]).await;
        assert_eq!(serve.base_url(), format!("http://127.0.0.1:{port}"));
        http_get(&serve.url("/health")).await.has_status(200);
    }

    #[tokio::test]
    async fn rejects_unknown_agent() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let output = catalog
            .command(&harness, &["serve", "no-such-agent"])
            .output()
            .await;
        assert_eq!(output.failure().exit_code(), 1, "\n{}", output.describe());
        assert!(output.stdout.is_empty(), "stdout: {:?}", output.stdout);
        output.stderr_contains("failed to serve agent \"no-such-agent\"");
    }
}

mod endpoint {
    use std::time::Duration;

    use futures::StreamExt;
    use reqwest::header::{ACCEPT, CONTENT_TYPE};
    use serde_json::Value;

    use crate::harness::{AcpAgent, Harness, WsClient};
    use crate::{CONNECTION_ID, ECHO, INITIALIZE, catalog_with, initialize, send, serve};

    #[tokio::test]
    async fn http_initialize() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let serve = serve(&harness, &catalog, &[]).await;
        let client = reqwest::Client::new();

        let response = send(
            client
                .post(serve.url("/acp"))
                .header(CONTENT_TYPE, "application/json")
                .body(INITIALIZE),
        )
        .await;
        assert_eq!(response.status(), 200);
        let id = response
            .headers()
            .get(CONNECTION_ID)
            .expect("initialize returns a connection id")
            .to_str()
            .unwrap()
            .to_string();
        assert!(!id.is_empty());

        let body: Value = response.json().await.expect("the body is JSON");
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 1);
        assert_eq!(body["result"]["protocolVersion"], 1);
    }

    #[tokio::test]
    async fn rejects_missing_content_type() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let serve = serve(&harness, &catalog, &[]).await;
        let response = send(
            reqwest::Client::new()
                .post(serve.url("/acp"))
                .body(INITIALIZE),
        )
        .await;
        assert_eq!(response.status(), 415);
        assert_eq!(
            response.text().await.unwrap(),
            "Content-Type must be application/json"
        );
    }

    #[tokio::test]
    async fn rejects_missing_accept() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let serve = serve(&harness, &catalog, &[]).await;
        let response = send(reqwest::Client::new().get(serve.url("/acp"))).await;
        assert_eq!(response.status(), 406);
        assert_eq!(
            response.text().await.unwrap(),
            "client must accept text/event-stream"
        );
    }

    #[tokio::test]
    async fn connection_lifecycle() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let serve = serve(&harness, &catalog, &[]).await;
        let client = reqwest::Client::new();
        let endpoint = serve.url("/acp");
        let id = initialize(&client, &endpoint).await;

        // A message carrying the connection id is accepted without a body.
        let accepted = send(
            client
                .post(&endpoint)
                .header(CONTENT_TYPE, "application/json")
                .header(CONNECTION_ID, &id)
                .body(ECHO),
        )
        .await;
        assert_eq!(accepted.status(), 202);

        // A message without a connection id, and not an initialize, is rejected.
        let missing = send(
            client
                .post(&endpoint)
                .header(CONTENT_TYPE, "application/json")
                .body(ECHO),
        )
        .await;
        assert_eq!(missing.status(), 400);
        assert_eq!(
            missing.text().await.unwrap(),
            "Acp-Connection-Id header required"
        );

        // An unknown connection id is not found.
        let unknown = send(
            client
                .post(&endpoint)
                .header(CONTENT_TYPE, "application/json")
                .header(CONNECTION_ID, "does-not-exist")
                .body(ECHO),
        )
        .await;
        assert_eq!(unknown.status(), 404);

        // DELETE without a connection id is rejected.
        let no_id = send(client.delete(&endpoint)).await;
        assert_eq!(no_id.status(), 400);

        // DELETE closes the connection; deleting it again is not found.
        let closed = send(client.delete(&endpoint).header(CONNECTION_ID, &id)).await;
        assert_eq!(closed.status(), 202);
        let again = send(client.delete(&endpoint).header(CONNECTION_ID, &id)).await;
        assert_eq!(again.status(), 404);
    }

    #[tokio::test]
    async fn sse_responses() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let serve = serve(&harness, &catalog, &[]).await;
        let client = reqwest::Client::new();
        let endpoint = serve.url("/acp");
        let id = initialize(&client, &endpoint).await;

        let sse = send(
            client
                .get(&endpoint)
                .header(ACCEPT, "text/event-stream")
                .header(CONNECTION_ID, &id),
        )
        .await;
        assert_eq!(sse.status(), 200);
        assert_eq!(
            sse.headers().get(CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );

        let mut events = sse.bytes_stream();
        let posted = send(
            client
                .post(&endpoint)
                .header(CONTENT_TYPE, "application/json")
                .header(CONNECTION_ID, &id)
                .body(ECHO),
        )
        .await;
        assert_eq!(posted.status(), 202);

        let frame = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(chunk) = events.next().await {
                let chunk = chunk.expect("the SSE chunk could be read");
                let text = String::from_utf8(chunk.to_vec()).expect("the SSE chunk is UTF-8");
                if text.contains("data: ") {
                    return text;
                }
            }
            panic!("the SSE stream ended without an event");
        })
        .await
        .expect("the SSE event did not arrive in time");
        let payload = frame
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("the SSE event carries data");
        let value: Value = serde_json::from_str(payload).expect("the SSE data is JSON");
        assert_eq!(value["id"], 2);
        assert_eq!(value["result"]["echo"], "ok");
    }

    #[tokio::test]
    async fn websocket() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let serve = serve(&harness, &catalog, &[]).await;

        let mut socket = WsClient::connect(&serve.ws_url("/acp")).await;
        assert!(
            socket.handshake_header(CONNECTION_ID).is_some(),
            "the upgrade carries a connection id"
        );
        let initialize: Value = serde_json::from_str(&socket.send_and_receive(INITIALIZE).await)
            .expect("the initialize answer is JSON");
        assert_eq!(initialize["result"]["protocolVersion"], 1);
        let echo: Value = serde_json::from_str(&socket.send_and_receive(ECHO).await)
            .expect("the echo answer is JSON");
        assert_eq!(echo["id"], 2);
        assert_eq!(echo["result"]["echo"], "ok");
        socket.close().await;
    }
}

mod health {
    use crate::harness::{AcpAgent, Harness, http_get};
    use crate::{catalog_with, serve};

    #[tokio::test]
    async fn ok() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let serve = serve(&harness, &catalog, &[]).await;
        let response = http_get(&serve.url("/health")).await;
        response.has_status(200);
        assert_eq!(response.body, "ok");
    }

    #[tokio::test]
    async fn disabled() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let serve = serve(&harness, &catalog, &["--no-health"]).await;
        http_get(&serve.url("/health")).await.has_status(404);
    }
}

mod readiness {
    use std::time::Duration;

    use crate::harness::{AcpAgent, Harness, http_get, http_post_json, wait_for_http_status};
    use crate::{INITIALIZE, catalog_with, serve};

    #[tokio::test]
    async fn ready() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let serve = serve(&harness, &catalog, &[]).await;
        let response = http_get(&serve.url("/readyz")).await;
        response.has_status(200);
        assert_eq!(response.body, "ready\n");
    }

    #[tokio::test]
    async fn failure_detail() {
        let harness = Harness::new();
        let failing = AcpAgent::Fail {
            code: 1,
            stderr: "could not find mock package".to_string(),
        };
        let catalog = catalog_with(&harness, &failing.script()).await;
        let serve = serve(&harness, &catalog, &[]).await;

        let response = http_post_json(&serve.url("/acp"), INITIALIZE).await;
        assert_eq!(response.status, 500, "{}", response.body);

        let readyz = wait_for_http_status(&serve.url("/readyz"), 503, Duration::from_secs(5)).await;
        assert!(
            readyz.body.contains("1 of 1 agent launches failed"),
            "{}",
            readyz.body
        );
        assert!(
            readyz.body.contains("could not find mock package"),
            "{}",
            readyz.body
        );
    }

    #[tokio::test]
    async fn disabled() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let serve = serve(&harness, &catalog, &["--no-readyz"]).await;
        http_get(&serve.url("/readyz")).await.has_status(404);
        assert!(
            !serve.stderr_text().contains("readiness probe"),
            "the startup line omits the probe: {}",
            serve.stderr_text()
        );
    }
}

mod paths {
    use reqwest::header::CONTENT_TYPE;

    use crate::harness::{AcpAgent, Harness, WsClient, http_get};
    use crate::{AGENT_ID, INITIALIZE, catalog_with, initialize, send, serve, serve_output};

    #[tokio::test]
    async fn custom_endpoint_path() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let serve = serve(&harness, &catalog, &["--path", "/rpc"]).await;
        let client = reqwest::Client::new();

        initialize(&client, &serve.url("/rpc")).await;
        http_get(&serve.url("/health")).await.has_status(200);
        http_get(&serve.url("/not-an-endpoint"))
            .await
            .has_status(404);
        let bare = send(
            client
                .post(serve.url("/acp"))
                .header(CONTENT_TYPE, "application/json")
                .body(INITIALIZE),
        )
        .await;
        assert_eq!(bare.status(), 404);
    }

    #[tokio::test]
    async fn mount_prefix() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let serve = serve(&harness, &catalog, &["--subpath", "/myapp"]).await;
        let client = reqwest::Client::new();

        let health = http_get(&serve.url("/myapp/health")).await;
        health.has_status(200);
        assert_eq!(health.body, "ok");
        http_get(&serve.url("/myapp/readyz")).await.has_status(200);
        initialize(&client, &serve.url("/myapp/acp")).await;

        http_get(&serve.url("/health")).await.has_status(404);
        let bare = send(
            client
                .post(serve.url("/acp"))
                .header(CONTENT_TYPE, "application/json")
                .body(INITIALIZE),
        )
        .await;
        assert_eq!(bare.status(), 404);

        let mut socket = WsClient::connect(&serve.ws_url("/myapp/acp")).await;
        let answer: serde_json::Value =
            serde_json::from_str(&socket.send_and_receive(INITIALIZE).await).expect("JSON");
        assert_eq!(answer["result"]["protocolVersion"], 1);
        socket.close().await;

        assert!(
            serve.stderr_text().contains("/myapp/acp"),
            "the startup line names the mount: {}",
            serve.stderr_text()
        );
    }

    #[tokio::test]
    async fn agent_sub_path() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let serve = serve(&harness, &catalog, &["--agent-sub-path"]).await;
        let client = reqwest::Client::new();

        http_get(&serve.url(&format!("/{AGENT_ID}/health")))
            .await
            .has_status(200);
        initialize(&client, &serve.url(&format!("/{AGENT_ID}/acp"))).await;
        http_get(&serve.url("/health")).await.has_status(404);
    }

    #[tokio::test]
    async fn rejects_invalid_mount() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let cases = [
            ("foo", "mount path must start with '/'"),
            ("/", "mount path cannot be '/'"),
            ("/x/", "mount path must not end with '/'"),
        ];
        for (value, message) in cases {
            let output = serve_output(&harness, &catalog, &["--subpath", value]).await;
            assert_eq!(output.failure().exit_code(), 1, "\n{}", output.describe());
            assert!(output.stdout.is_empty(), "stdout: {:?}", output.stdout);
            output.stderr_contains(message);
        }
    }

    #[tokio::test]
    async fn rejects_invalid_endpoint_path() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let cases = [
            ("acp", "ACP endpoint path must start with '/'"),
            ("/", "ACP endpoint path cannot be '/'"),
            (
                "/health",
                "ACP endpoint path conflicts with the health endpoint",
            ),
            (
                "/readyz",
                "ACP endpoint path conflicts with the readiness endpoint",
            ),
        ];
        for (value, message) in cases {
            let output = serve_output(&harness, &catalog, &["--path", value]).await;
            assert_eq!(output.failure().exit_code(), 1, "\n{}", output.describe());
            assert!(output.stdout.is_empty(), "stdout: {:?}", output.stdout);
            output.stderr_contains(message);
        }
    }

    #[tokio::test]
    async fn rejects_conflicting_mounts() {
        let harness = Harness::new();
        let output = harness
            .run(&["serve", AGENT_ID, "--agent-sub-path", "--subpath", "/x"])
            .await;
        assert_eq!(output.failure().exit_code(), 2, "\n{}", output.describe());
        output.stderr_contains("cannot be used with");
    }
}

mod cors {
    use reqwest::header::{ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_REQUEST_METHOD, ORIGIN};

    use crate::harness::{AcpAgent, Harness, WsClient, http_get};
    use crate::{AGENT_ID, catalog_with, send, serve};

    #[tokio::test]
    async fn disabled_by_default() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let serve = serve(&harness, &catalog, &[]).await;

        let rejected =
            WsClient::try_connect_with_origin(&serve.ws_url("/acp"), "https://example.com")
                .await
                .unwrap_err();
        assert_eq!(rejected, 403);
        let response = http_get(&serve.url("/health")).await;
        assert_eq!(response.header("access-control-allow-origin"), None);
    }

    #[tokio::test]
    async fn allowed_origins() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let serve = serve(
            &harness,
            &catalog,
            &[
                "--cors-origin",
                "https://a.example",
                "--cors-origin",
                "https://b.example",
            ],
        )
        .await;
        let client = reqwest::Client::new();

        let allowed = preflight(&client, &serve.url("/acp"), "https://a.example").await;
        assert_eq!(allowed.status(), 200);
        assert_eq!(
            allowed.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
            "https://a.example"
        );

        let disallowed = preflight(&client, &serve.url("/acp"), "https://c.example").await;
        assert_eq!(disallowed.status(), 200);
        assert!(
            disallowed
                .headers()
                .get(ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );

        let listed = send(
            client
                .get(serve.url("/health"))
                .header(ORIGIN, "https://b.example"),
        )
        .await;
        assert_eq!(
            listed.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
            "https://b.example"
        );
    }

    #[tokio::test]
    async fn allow_any_origin() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let serve = serve(&harness, &catalog, &["--allow-any-origin"]).await;

        let socket =
            WsClient::try_connect_with_origin(&serve.ws_url("/acp"), "https://weird.example")
                .await
                .expect("every origin is allowed");
        socket.close().await;

        let response = send(
            reqwest::Client::new()
                .get(serve.url("/health"))
                .header(ORIGIN, "https://weird.example"),
        )
        .await;
        assert_eq!(
            response.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
            "*"
        );
    }

    #[tokio::test]
    async fn rejects_conflicting_options() {
        let harness = Harness::new();
        let output = harness
            .run(&[
                "serve",
                AGENT_ID,
                "--cors-origin",
                "https://example.com",
                "--allow-any-origin",
            ])
            .await;
        assert_eq!(output.failure().exit_code(), 2, "\n{}", output.describe());
        output.stderr_contains("cannot be used with");
    }

    async fn preflight(client: &reqwest::Client, url: &str, origin: &str) -> reqwest::Response {
        send(
            client
                .request(reqwest::Method::OPTIONS, url)
                .header(ORIGIN, origin)
                .header(ACCESS_CONTROL_REQUEST_METHOD, "POST"),
        )
        .await
    }
}

mod limit {
    use reqwest::header::CONTENT_TYPE;

    use crate::harness::{AcpAgent, Harness, WsClient, http_get};
    use crate::{CONNECTION_ID, INITIALIZE, catalog_with, initialize, send, serve};

    #[tokio::test]
    async fn rejects_overload() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let serve = serve(&harness, &catalog, &["--max-processes", "1"]).await;
        let client = reqwest::Client::new();
        let endpoint = serve.url("/acp");
        let id = initialize(&client, &endpoint).await;

        let overloaded = send(
            client
                .post(&endpoint)
                .header(CONTENT_TYPE, "application/json")
                .body(INITIALIZE),
        )
        .await;
        assert_eq!(overloaded.status(), 503);
        assert_eq!(
            overloaded.text().await.unwrap(),
            "agent process capacity exhausted\n"
        );

        http_get(&serve.url("/health")).await.has_status(200);
        http_get(&serve.url("/readyz")).await.has_status(200);

        let closed = send(client.delete(&endpoint).header(CONNECTION_ID, &id)).await;
        assert_eq!(closed.status(), 202);
        let reopened = send(
            client
                .post(&endpoint)
                .header(CONTENT_TYPE, "application/json")
                .body(INITIALIZE),
        )
        .await;
        assert_eq!(reopened.status(), 200);
    }

    #[tokio::test]
    async fn rejects_websocket_overload() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let serve = serve(&harness, &catalog, &["--max-processes", "1"]).await;
        let client = reqwest::Client::new();
        let endpoint = serve.url("/acp");
        let id = initialize(&client, &endpoint).await;

        assert_eq!(
            WsClient::try_connect(&serve.ws_url("/acp"))
                .await
                .unwrap_err(),
            503
        );

        let closed = send(client.delete(&endpoint).header(CONNECTION_ID, &id)).await;
        assert_eq!(closed.status(), 202);
    }
}

mod shutdown {
    use std::time::Duration;

    use futures::StreamExt;
    use reqwest::header::ACCEPT;

    use crate::harness::{AcpAgent, Harness};
    use crate::{CONNECTION_ID, catalog_with, initialize, send, serve};

    #[tokio::test]
    async fn termination_signal() {
        for signal in ["TERM", "INT"] {
            let harness = Harness::new();
            let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
            let mut serve = serve(&harness, &catalog, &[]).await;
            serve.signal(signal);
            let status = serve.wait_for_exit().await;
            assert!(
                status.success(),
                "SIG{signal} must exit the server cleanly, got {status:?}"
            );
        }
    }

    #[tokio::test]
    async fn closes_active_stream() {
        let harness = Harness::new();
        let catalog = catalog_with(&harness, &AcpAgent::Echo.script()).await;
        let mut serve = serve(&harness, &catalog, &[]).await;
        let client = reqwest::Client::new();
        let endpoint = serve.url("/acp");
        let id = initialize(&client, &endpoint).await;

        let sse = send(
            client
                .get(&endpoint)
                .header(ACCEPT, "text/event-stream")
                .header(CONNECTION_ID, &id),
        )
        .await;
        assert_eq!(sse.status(), 200);
        let mut events = sse.bytes_stream();

        serve.signal("TERM");
        let closed = tokio::time::timeout(Duration::from_secs(6), async {
            while let Some(chunk) = events.next().await {
                if chunk.is_err() {
                    return;
                }
            }
        })
        .await;
        assert!(closed.is_ok(), "the server did not close the open stream");
        assert!(serve.wait_for_exit().await.success());
    }
}
