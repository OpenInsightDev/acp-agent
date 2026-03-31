//! Unix domain socket transport that proxies stdin/stdout bytes without framing.
//! Only a single client is accepted so transports that need multiplexing should
//! use the WebSocket or HTTP/2 implementations instead.
#![cfg(unix)]

use std::path::Path;
use std::path::PathBuf;
use std::process::ExitStatus;

use anyhow::{Context, Result, bail};
use tokio::net::{UnixListener, UnixStream};

use crate::runtime::prepare::{CommandSpec, PreparedCommand};
use crate::runtime::transports::raw::serve_raw_stream_connection;

/// Accepts one Unix-domain-socket client and shuffles raw bytes between it and the agent.
pub async fn serve_uds(
    prepared: PreparedCommand,
    subject: &str,
    path: &Path,
) -> Result<ExitStatus> {
    if path.exists() {
        bail!(
            "refusing to bind Unix socket at {}: path already exists",
            path.display()
        );
    }

    let mut socket_path_guard = SocketPathGuard::new(path.to_path_buf());
    let listener = UnixListener::bind(path)
        .with_context(|| format!("failed to bind Unix socket listener at {}", path.display()))?;
    socket_path_guard.mark_created();
    eprintln!(
        "Running {} over unix://{} (raw stdio stream transport)",
        subject,
        path.display()
    );

    let (socket, _peer_address) = listener.accept().await.with_context(|| {
        format!(
            "failed to accept Unix socket connection on {}",
            path.display()
        )
    })?;
    eprintln!("Accepted connection on {}", path.display());

    let _temp_dir = prepared.temp_dir;
    serve_uds_connection(prepared.spec, subject, socket).await
}

struct SocketPathGuard {
    path: PathBuf,
    created: bool,
}

impl SocketPathGuard {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            created: false,
        }
    }

    fn mark_created(&mut self) {
        self.created = true;
    }
}

impl Drop for SocketPathGuard {
    fn drop(&mut self) {
        if !self.created {
            return;
        }

        let _ = std::fs::remove_file(&self.path);
    }
}

async fn serve_uds_connection(
    spec: CommandSpec,
    subject: &str,
    socket: UnixStream,
) -> Result<ExitStatus> {
    serve_raw_stream_connection(spec, subject, socket).await
}
