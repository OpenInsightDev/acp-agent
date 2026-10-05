use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use tokio::time::sleep;

use super::{DAEMON_START_TIMEOUT, Harness};

/// A foreground `acp-agent daemon` owned by one test; `server start` would
/// otherwise auto-start a daemon that outlives the test. Diagnostics go to a log
/// file rather than a pipe, which keeps them readable without a draining thread.
pub struct Daemon {
    child: Child,
    socket: PathBuf,
    stderr_log: PathBuf,
}

impl Daemon {
    pub(crate) async fn start(harness: &Harness) -> Self {
        let stderr_log = harness.root().join("daemon.stderr.log");
        let stderr = File::create(&stderr_log)
            .unwrap_or_else(|error| panic!("failed to create {}: {error}", stderr_log.display()));
        let child = harness
            .command(&["daemon"])
            .spawn(Stdio::null(), Stdio::from(stderr));
        let daemon = Self {
            child,
            socket: harness.socket_path().to_path_buf(),
            stderr_log,
        };
        daemon.wait_until_ready(harness).await;
        daemon
    }

    /// `server list` never auto-starts a daemon, so a probe can only observe the
    /// daemon this harness started.
    async fn wait_until_ready(&self, harness: &Harness) {
        let deadline = Instant::now() + DAEMON_START_TIMEOUT;
        loop {
            let probe = harness.run(&["server", "list"]).await;
            if probe.status.success() {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "daemon did not become ready within {DAEMON_START_TIMEOUT:?}\n{}\ndaemon stderr:\n{}",
                    probe.describe(),
                    self.stderr_text().trim_end_matches('\n')
                );
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    pub fn stderr_text(&self) -> String {
        std::fs::read_to_string(&self.stderr_log).unwrap_or_default()
    }

    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}
