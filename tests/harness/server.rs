use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Serves a directory of fixture files over loopback HTTP, for inputs a spawned
/// process fetches itself (a registry payload, a binary archive).
///
/// It also counts requests per path, so a test can assert that a warm cache did
/// not re-fetch an archive.
pub struct FixtureServer {
    address: SocketAddr,
    root: PathBuf,
    counts: Arc<Mutex<HashMap<String, usize>>>,
    task: JoinHandle<()>,
}

impl FixtureServer {
    pub async fn start(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind the fixture server");
        let address = listener
            .local_addr()
            .expect("failed to read the fixture server address");
        let counts = Arc::new(Mutex::new(HashMap::new()));
        let state = ServerState {
            root: root.clone(),
            counts: Arc::clone(&counts),
        };
        let router = Router::new().fallback(any(serve_file)).with_state(state);
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self {
            address,
            root,
            counts,
            task,
        }
    }

    /// How many requests have been served for `relative` so far. Paths are
    /// matched with any leading `/` trimmed, the same way [`Self::url`] names
    /// them.
    pub fn request_count(&self, relative: &str) -> usize {
        let relative = relative.trim_start_matches('/');
        self.counts
            .lock()
            .expect("fixture server request counts")
            .get(relative)
            .copied()
            .unwrap_or(0)
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn url(&self, relative: &str) -> String {
        format!("{}/{}", self.base_url(), relative.trim_start_matches('/'))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone)]
struct ServerState {
    root: PathBuf,
    counts: Arc<Mutex<HashMap<String, usize>>>,
}

async fn serve_file(State(state): State<ServerState>, uri: Uri) -> Response {
    let relative = uri.path().trim_start_matches('/');
    // A client normalizes dot segments before sending, a raw request does not.
    let safe = !relative.is_empty()
        && relative
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..");
    if !safe {
        return StatusCode::NOT_FOUND.into_response();
    }
    {
        let mut counts = state.counts.lock().expect("fixture server request counts");
        *counts.entry(relative.to_string()).or_default() += 1;
    }
    match tokio::fs::read(state.root.join(relative)).await {
        Ok(bytes) => {
            let content_type = if relative.ends_with(".json") {
                "application/json"
            } else {
                "application/octet-stream"
            };
            ([(header::CONTENT_TYPE, content_type)], bytes).into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
