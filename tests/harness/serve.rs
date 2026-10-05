use std::fs::File;
use std::path::PathBuf;
use std::process::{Child, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use tokio::time::sleep;

use super::{Command, Harness};

/// How long a `serve` process may take to print its address, answer, and stop.
///
/// Generous on purpose: the whole suite runs its targets in parallel, so a
/// loaded machine can stretch a launch or the shutdown drain well past what an
/// idle run takes.
pub const SERVE_START_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A foreground `acp-agent serve` owned by one test.
///
/// The bound port is discovered from the address the process prints on standard
/// error, so a case never has to guess a port. Standard output and standard
/// error are captured to files under the harness root, which a case can read and
/// which keep diagnostics visible without a draining thread.
pub struct Serve {
    child: Child,
    base_url: String,
    stdout_log: PathBuf,
    stderr_log: PathBuf,
}

impl Serve {
    /// Starts `command` (an `acp-agent serve` invocation) and waits until the
    /// address it prints is reachable.
    pub(crate) async fn start(harness: &Harness, command: Command) -> Self {
        let stdout_log = harness.root().join("serve.stdout.log");
        let stderr_log = harness.root().join("serve.stderr.log");
        let stdout = File::create(&stdout_log)
            .unwrap_or_else(|error| panic!("failed to create {}: {error}", stdout_log.display()));
        let stderr = File::create(&stderr_log)
            .unwrap_or_else(|error| panic!("failed to create {}: {error}", stderr_log.display()));
        let child = command.spawn(Stdio::from(stdout), Stdio::from(stderr));
        let mut serve = Self {
            child,
            base_url: String::new(),
            stdout_log,
            stderr_log,
        };
        let base_url = serve.wait_for_address().await;
        wait_until_connectable(&base_url).await;
        serve.base_url = base_url;
        serve
    }

    /// Base URL of the server, `http://host:port` with no trailing path.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Absolute URL for `path` on this server, e.g. `url("/health")`.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// `ws://` URL for `path` on this server, for the WebSocket transport.
    pub fn ws_url(&self, path: &str) -> String {
        format!("ws://{}{path}", &self.base_url["http://".len()..])
    }

    pub fn stdout_text(&self) -> String {
        std::fs::read_to_string(&self.stdout_log).unwrap_or_default()
    }

    pub fn stderr_text(&self) -> String {
        std::fs::read_to_string(&self.stderr_log).unwrap_or_default()
    }

    /// Waits until standard error contains `needle`.
    ///
    /// The address line and the readiness line are separate writes, so a case
    /// that inspects the whole stream waits for the later one instead of racing
    /// the process.
    pub async fn wait_for_stderr(&self, needle: &str) {
        let deadline = Instant::now() + SERVE_START_TIMEOUT;
        loop {
            if self.stderr_text().contains(needle) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "serve did not report {needle:?} within {SERVE_START_TIMEOUT:?}\nstderr:\n{}",
                self.stderr_text().trim_end_matches('\n')
            );
            sleep(POLL_INTERVAL).await;
        }
    }

    /// Reads the address line the process printed, failing if it never appeared.
    async fn wait_for_address(&mut self) -> String {
        let deadline = Instant::now() + SERVE_START_TIMEOUT;
        loop {
            if let Some(base_url) = first_url(&self.stderr_text()) {
                return base_url;
            }
            if self
                .child
                .try_wait()
                .expect("failed to poll the serve process")
                .is_some()
            {
                panic!(
                    "serve exited before printing its address\nstderr:\n{}",
                    self.stderr_text().trim_end_matches('\n')
                );
            }
            assert!(
                Instant::now() < deadline,
                "serve did not print an address within {SERVE_START_TIMEOUT:?}\nstderr:\n{}",
                self.stderr_text().trim_end_matches('\n')
            );
            sleep(POLL_INTERVAL).await;
        }
    }

    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Delivers `signal`, named as `/bin/kill` takes it (e.g. `TERM`), to the
    /// serve process.
    pub fn signal(&self, signal: &str) {
        let status = std::process::Command::new("/bin/kill")
            .arg(format!("-{signal}"))
            .arg(self.child.id().to_string())
            .status()
            .unwrap_or_else(|error| panic!("failed to run /bin/kill: {error}"));
        assert!(status.success(), "failed to deliver SIG{signal}");
    }

    /// Waits for the serve process to exit and returns its status.
    pub async fn wait_for_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + SERVE_START_TIMEOUT;
        loop {
            if let Some(status) = self
                .child
                .try_wait()
                .expect("failed to poll the serve process")
            {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "serve did not exit within {SERVE_START_TIMEOUT:?}"
            );
            sleep(POLL_INTERVAL).await;
        }
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        self.stop();
    }
}

/// First `http://host:port` URL in `text`, stripped of its path.
fn first_url(text: &str) -> Option<String> {
    let start = text.find("http://")?;
    let after = &text[start + "http://".len()..];
    let end = after
        .find(|ch: char| ch == '/' || ch.is_whitespace())
        .unwrap_or(after.len());
    Some(format!("http://{}", &after[..end]))
}

async fn wait_until_connectable(base_url: &str) {
    let authority = base_url
        .strip_prefix("http://")
        .expect("the address has an http scheme");
    let deadline = Instant::now() + SERVE_START_TIMEOUT;
    loop {
        if TcpStream::connect(authority).await.is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{base_url} was not reachable within {SERVE_START_TIMEOUT:?}"
        );
        sleep(POLL_INTERVAL).await;
    }
}
