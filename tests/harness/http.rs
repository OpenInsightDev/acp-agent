use std::net::TcpListener;
use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderName};
use serde_json::Value;
use tokio::time::{sleep, timeout as deadline};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: String,
}

impl HttpResponse {
    pub fn success(&self) -> &Self {
        assert!(
            (200..300).contains(&self.status),
            "expected a 2xx response, got {}\n{}",
            self.status,
            self.body
        );
        self
    }

    pub fn has_status(&self, status: u16) -> &Self {
        assert_eq!(
            self.status, status,
            "unexpected response body:\n{}",
            self.body
        );
        self
    }

    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("response was not JSON: {error}\n{}", self.body))
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        let name = HeaderName::from_bytes(name.as_bytes()).expect("invalid header name");
        self.headers
            .get(&name)
            .and_then(|value| value.to_str().ok())
    }
}

pub async fn http_get(url: &str) -> HttpResponse {
    send(client().get(url), url).await
}

pub async fn http_post_json(url: &str, body: &str) -> HttpResponse {
    let request = client()
        .post(url)
        .header("content-type", "application/json")
        .body(body.to_string());
    send(request, url).await
}

pub async fn wait_for_http_status(url: &str, expected: u16, wait: Duration) -> HttpResponse {
    let start = Instant::now();
    loop {
        let response = http_get(url).await;
        if response.status == expected {
            return response;
        }
        if start.elapsed() >= wait {
            panic!(
                "{url} did not answer {expected} within {wait:?}; last status {}, body:\n{}",
                response.status, response.body
            );
        }
        sleep(POLL_INTERVAL).await;
    }
}

/// The port is released before it is returned, so a caller that needs a concrete
/// port must tolerate a rare bind failure; `--port 0` is more reliable.
pub fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind a loopback port");
    listener
        .local_addr()
        .expect("failed to read the bound port")
        .port()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("failed to build the harness HTTP client")
}

async fn send(request: reqwest::RequestBuilder, url: &str) -> HttpResponse {
    let response = deadline(REQUEST_TIMEOUT, request.send())
        .await
        .unwrap_or_else(|_| panic!("{url} did not answer within {REQUEST_TIMEOUT:?}"))
        .unwrap_or_else(|error| panic!("{url} failed: {error}"));
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response
        .text()
        .await
        .unwrap_or_else(|error| panic!("{url} body could not be read: {error}"));
    HttpResponse {
        status,
        headers,
        body,
    }
}
