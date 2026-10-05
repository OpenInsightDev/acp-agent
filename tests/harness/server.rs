use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Serves a directory of fixture files over loopback HTTP, for inputs a spawned
/// process fetches itself (a registry payload, a binary archive).
pub struct FixtureServer {
    address: SocketAddr,
    root: PathBuf,
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
        let router = Router::new()
            .fallback(any(serve_file))
            .with_state(root.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self {
            address,
            root,
            task,
        }
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

async fn serve_file(State(root): State<PathBuf>, uri: Uri) -> Response {
    let relative = uri.path().trim_start_matches('/');
    // A client normalizes dot segments before sending, a raw request does not.
    let safe = !relative.is_empty()
        && relative
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..");
    if !safe {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tokio::fs::read(root.join(relative)).await {
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
