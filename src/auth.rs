use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::Response,
};
use subtle::ConstantTimeEq;

use crate::config::Config;

fn token_matches(expected: &str, presented: &str) -> bool {
    // Length is not secret; the bytes are. Comparing only equal-length inputs
    // keeps the comparison constant-time over the part that matters.
    expected.len() == presented.len() && expected.as_bytes().ct_eq(presented.as_bytes()).into()
}

/// Reject requests that fail bearer auth or carry a disallowed `Origin`.
///
/// Origin checking is required by the MCP spec to defeat DNS rebinding: a page
/// in someone's browser must not be able to drive this server just because it
/// can reach the address. Absent `Origin` means a non-browser client, which is
/// what ChatGPT is.
pub async fn guard(
    State(config): State<Arc<Config>>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if let Some(origin) = request.headers().get(header::ORIGIN) {
        let origin = origin.to_str().map_err(|_| StatusCode::FORBIDDEN)?;
        if !config.allowed_origins.iter().any(|a| a == origin) {
            tracing::warn!(%origin, "rejected request with disallowed Origin");
            return Err(StatusCode::FORBIDDEN);
        }
    }

    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;

    if !token_matches(&config.token, presented.trim()) {
        tracing::warn!("rejected request with invalid bearer token");
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_the_exact_token() {
        assert!(token_matches("supersecrettoken", "supersecrettoken"));
        assert!(!token_matches("supersecrettoken", "supersecrettoke"));
        assert!(!token_matches("supersecrettoken", "supersecrettokenX"));
        assert!(!token_matches("supersecrettoken", ""));
    }
}
