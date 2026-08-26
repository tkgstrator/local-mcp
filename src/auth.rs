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
/// Origin checking defeats DNS rebinding: a page in someone's browser must not
/// be able to drive this server just because it can reach the address. The
/// bearer token already covers that — a browser will not attach an
/// `Authorization` header on its own — so the check is opt-in. Enabling it by
/// default only turned away legitimate clients that do send an `Origin`, which
/// ChatGPT turns out to be.
pub async fn guard(
    State(config): State<Arc<Config>>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if !config.allowed_origins.is_empty()
        && let Some(origin) = request.headers().get(header::ORIGIN)
    {
        let origin = origin.to_str().map_err(|_| StatusCode::FORBIDDEN)?;
        if !config.allowed_origins.iter().any(|a| a == origin) {
            tracing::warn!(%origin, "rejected request with disallowed Origin");
            return Err(StatusCode::FORBIDDEN);
        }
    }

    let path = request.uri().path().to_string();
    let Some(presented) = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        // Logged rather than silently refused: a client probing without
        // credentials looks identical to no traffic at all otherwise.
        tracing::warn!(%path, "rejected request with no bearer token");
        return Err(StatusCode::UNAUTHORIZED);
    };

    if !token_matches(&config.token, presented.trim()) {
        tracing::warn!(%path, "rejected request with invalid bearer token");
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
