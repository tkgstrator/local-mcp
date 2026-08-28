use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use subtle::ConstantTimeEq;

use crate::{config::Config, oauth::OAuth};

#[derive(Clone)]
pub struct AuthState {
    pub config: Arc<Config>,
    /// Present only when a public URL is configured, since OAuth metadata has
    /// to advertise absolute URLs.
    pub oauth: Option<Arc<OAuth>>,
}

fn token_matches(expected: &str, presented: &str) -> bool {
    // Length is not secret; the bytes are. Comparing only equal-length inputs
    // keeps the comparison constant-time over the part that matters.
    expected.len() == presented.len() && expected.as_bytes().ct_eq(presented.as_bytes()).into()
}

/// Reject requests that fail bearer auth.
///
/// There is deliberately no `Origin` check. It would only defend against a page
/// in someone's browser driving this server, which the bearer token already
/// prevents — a browser does not attach an `Authorization` header on its own.
/// Enforcing it turned away legitimate clients that do send an `Origin`, which
/// ChatGPT turns out to be.
pub async fn guard(
    State(state): State<AuthState>,
    request: Request,
    next: Next,
) -> Result<Response, Response> {
    let unauthorized = || {
        let mut response = StatusCode::UNAUTHORIZED.into_response();
        // RFC 9728: tells a client where to find the metadata describing how to
        // authenticate. Without it, a client that has no token cannot discover
        // that an OAuth flow is on offer.
        if let Some(oauth) = &state.oauth
            && let Ok(value) = format!(
                r#"Bearer resource_metadata="{}""#,
                oauth.resource_metadata_url()
            )
            .parse()
        {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, value);
        }
        response
    };

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
        return Err(unauthorized());
    };

    let presented = presented.trim();
    // The configured token is accepted directly so clients that can hold a
    // secret skip the OAuth round trip entirely.
    let accepted = token_matches(&state.config.token, presented)
        || state.oauth.as_ref().is_some_and(|oauth| {
            // A database that cannot be read denies everyone, which is the safe
            // direction — but it is indistinguishable from a wrong token in the
            // log unless the real reason is recorded here.
            oauth.token_is_valid(presented).unwrap_or_else(|error| {
                tracing::error!(%error, "cannot check the presented token");
                false
            })
        });

    if !accepted {
        tracing::warn!(%path, "rejected request with invalid bearer token");
        return Err(unauthorized());
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
