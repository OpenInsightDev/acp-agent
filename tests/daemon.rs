//! End-to-end tests for [`Daemon.md`](Daemon.md).
//!
//! Every case runs the built `acp-agent daemon` binary against a harness-owned
//! socket, so the assertions observe the process boundary — exit status, stderr,
//! and the socket file — rather than the crate's internals.
//! The health case hand-rolls the control protocol's length-prefixed JSON frame,
//! because the crate's protocol module is not part of its public surface.

mod harness;

use std::path::Path;
use std::process::{Child, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use harness::Command;

/// The control-protocol version the daemon implements.
const PROTOCOL_VERSION: u32 = 2;
/// The environment variable that overrides the daemon's control socket.
const SOCKET_ENV: &str = "ACP_AGENT_DAEMON_SOCKET";
/// How long a daemon may take to answer before a case fails.
const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Poll interval while waiting on the daemon.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A daemon a case starts itself, so it can deliver `SIGTERM`/`SIGINT` and wait
/// for the exit; the harness `Daemon` guard only sends `SIGKILL`.
struct Foreground {
    child: Child,
}

impl Foreground {
    fn spawn(command: Command) -> Self {
        Self {
            child: command.spawn(Stdio::null(), Stdio::null()),
        }
    }

    /// Delivers `signal`, named as `/bin/kill` takes it (e.g. `TERM`), to the
    /// daemon.
    fn signal(&self, signal: &str) {
        let status = std::process::Command::new("/bin/kill")
            .arg(format!("-{signal}"))
            .arg(self.child.id().to_string())
            .status()
            .unwrap_or_else(|error| panic!("failed to run /bin/kill: {error}"));
        assert!(status.success(), "failed to deliver SIG{signal}");
    }

    async fn wait_for_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("failed to poll the daemon") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "the daemon did not exit within {READY_TIMEOUT:?}"
            );
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

impl Drop for Foreground {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Sends one health request over a fresh connection and returns the decoded
/// response, or `None` when no daemon is answering yet.
async fn health_response(socket: &Path) -> Option<Value> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::UnixStream::connect(socket).await.ok()?;
    let payload = serde_json::to_vec(&json!({ "version": PROTOCOL_VERSION, "command": "Health" }))
        .expect("a health request serializes");
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await
        .ok()?;
    stream.write_all(&payload).await.ok()?;
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length).await.ok()?;
    let mut body = vec![0_u8; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut body).await.ok()?;
    serde_json::from_slice(&body).ok()
}

/// Polls `socket` until the daemon behind it answers a health request.
async fn wait_until_ready(socket: &Path) {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if health_response(socket).await.is_some() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no daemon answered health on {} within {READY_TIMEOUT:?}",
            socket.display()
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

mod socket {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use crate::harness::Harness;
    use crate::{Foreground, SOCKET_ENV, wait_until_ready};

    #[tokio::test]
    async fn rejects_invalid_override() {
        let harness = Harness::new();
        let too_long = format!("/{}", "x".repeat(104));
        let cases: [(&str, &str); 4] = [
            ("", "must not be empty"),
            ("daemon.sock", "must be absolute"),
            ("/", "must name a socket file"),
            (too_long.as_str(), "too long"),
        ];
        for (value, message) in cases {
            let output = harness
                .command(&["daemon"])
                .env(SOCKET_ENV, value)
                .output()
                .await;
            output.failure().stderr_contains(message);
        }
        assert!(
            !harness.socket_path().exists(),
            "a rejected override creates no socket"
        );
    }

    #[tokio::test]
    async fn rejects_public_parent() {
        let harness = Harness::new();
        let parent = harness.socket_path().with_file_name("public");
        fs::create_dir(&parent).expect("failed to create the socket parent");
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755))
            .expect("failed to relax the socket parent");
        let socket = parent.join("daemon.sock");
        let output = harness
            .command(&["daemon"])
            .env(SOCKET_ENV, &socket)
            .output()
            .await;
        output.failure().stderr_contains("must have mode 0700");
        assert!(!socket.exists(), "a rejected parent creates no socket");
    }

    #[tokio::test]
    async fn creates_private_parent() {
        let harness = Harness::new();
        // Beside the harness socket, whose directory is already under the
        // socket-path length limit.
        let socket = harness.socket_path().with_file_name("nested/daemon.sock");
        let _daemon = Foreground::spawn(harness.command(&["daemon"]).env(SOCKET_ENV, &socket));
        wait_until_ready(&socket).await;
        assert!(socket.exists(), "the daemon must bind the requested socket");
        let mode = fs::metadata(socket.parent().expect("the socket has a parent"))
            .expect("the socket parent exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "the daemon creates its socket parent as 0700");
    }

    #[tokio::test]
    async fn default_path() {
        let harness = Harness::new();
        let socket = harness.default_socket_path();
        let _daemon = Foreground::spawn(harness.command(&["daemon"]).env_remove(SOCKET_ENV));
        wait_until_ready(&socket).await;
        assert!(socket.exists(), "the daemon binds {}", socket.display());
        assert!(
            socket.starts_with(harness.cache_root()),
            "the default socket lives under the cache root: {}",
            socket.display()
        );
    }
}

mod ownership {
    use crate::harness::Harness;

    #[tokio::test]
    async fn single_owner() {
        let harness = Harness::new();
        let _owner = harness.daemon().await;
        let second = harness.run(&["daemon"]).await;
        second.failure().stderr_contains("already in use");
        harness.run(&["server", "list"]).await.success();
        assert!(
            harness.socket_path().exists(),
            "the live daemon keeps its socket"
        );
    }
}

mod recovery {
    use std::fs;

    use crate::harness::Harness;

    #[tokio::test]
    async fn stale_socket() {
        let harness = Harness::new();
        let mut killed = harness.daemon().await;
        killed.stop();
        assert!(
            harness.socket_path().exists(),
            "SIGKILL leaves the socket file behind"
        );
        let _recovered = harness.daemon().await;
        harness.run(&["server", "list"]).await.success();
    }

    #[tokio::test]
    async fn unrelated_entry() {
        let file = Harness::new();
        fs::write(file.socket_path(), b"not a socket").expect("failed to write the socket path");
        file.run(&["daemon"])
            .await
            .failure()
            .stderr_contains("already in use");
        assert_eq!(
            fs::read(file.socket_path()).expect("the file stays"),
            b"not a socket"
        );

        let directory = Harness::new();
        fs::create_dir(directory.socket_path())
            .expect("failed to make the socket path a directory");
        directory
            .run(&["daemon"])
            .await
            .failure()
            .stderr_contains("already in use");
        assert!(directory.socket_path().is_dir(), "the directory stays");
    }
}

mod control {
    use crate::harness::Harness;
    use crate::{PROTOCOL_VERSION, health_response};

    #[tokio::test]
    async fn health() {
        let harness = Harness::new();
        let _daemon = harness.daemon().await;
        let response = health_response(harness.socket_path())
            .await
            .expect("the daemon answers health");
        assert_eq!(response["status"], "ok");
        assert_eq!(
            response["version"].as_u64(),
            Some(u64::from(PROTOCOL_VERSION))
        );
        assert_eq!(
            response["result"]["data"]["protocol_version"].as_u64(),
            Some(u64::from(PROTOCOL_VERSION))
        );
    }
}

mod lifecycle {
    use crate::harness::Harness;
    use crate::{Foreground, wait_until_ready};

    #[tokio::test]
    async fn termination_signal() {
        for signal in ["TERM", "INT"] {
            let harness = Harness::new();
            let mut daemon = Foreground::spawn(harness.command(&["daemon"]));
            wait_until_ready(harness.socket_path()).await;
            daemon.signal(signal);
            let status = daemon.wait_for_exit().await;
            assert!(
                status.success(),
                "SIG{signal} must exit the daemon cleanly, got {status:?}"
            );
            assert!(
                !harness.socket_path().exists(),
                "SIG{signal} must remove the socket"
            );
        }
    }
}
