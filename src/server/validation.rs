use anyhow::{Context, Result, bail};
use axum::http::Uri;

pub(super) fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("server name must contain only letters, digits, '.', '-' or '_'");
    }
    Ok(())
}

pub(super) fn validate_agent_id(id: &str) -> bool {
    !id.trim().is_empty()
}

pub(super) fn validate_route(route: &str) -> Result<()> {
    if !route.starts_with('/') || route == "/" || route.ends_with('/') {
        bail!("agent route must start with '/', cannot be '/', and must not end with '/'");
    }
    if route.contains(['?', '#']) || route.split('/').any(|part| part == "..") {
        bail!("agent route contains an invalid path segment");
    }
    let uri: Uri = route
        .parse()
        .context("agent route is not a valid URI path")?;
    if uri.path() != route || uri.query().is_some() {
        bail!("agent route is not a valid URI path");
    }
    Ok(())
}
