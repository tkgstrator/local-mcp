//! A minimal OAuth 2.1 authorization server, colocated with the resource it
//! protects.
//!
//! Clients that cannot be handed a static token — ChatGPT among them — will
//! only talk to a server that advertises OAuth. Nothing here federates identity:
//! the consent screen asks for `LOCAL_MCP_TOKEN`, so the shared secret still
//! decides who gets in. The flow exists to package that secret in the shape
//! those clients insist on.

use std::{
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::Result;
use axum::{
    Form, Json,
    extract::{Query, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::store::{PendingCode, Store};

/// Long enough to complete a consent screen, short enough that a leaked code in
/// a proxy log is worthless by the time anyone reads it.
const CODE_TTL: Duration = Duration::from_secs(600);
const TOKEN_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 30);

pub struct OAuth {
    issuer: String,
    consent_secret: String,
    store: Store,
}

impl OAuth {
    pub fn new(issuer: String, consent_secret: String, database: &Path) -> Result<Self> {
        Ok(Self {
            issuer,
            consent_secret,
            store: Store::open(database)?,
        })
    }

    /// Where a client should look for the metadata that describes this flow.
    /// Sent on every 401 so an unauthenticated client can discover it.
    pub fn resource_metadata_url(&self) -> String {
        format!("{}/.well-known/oauth-protected-resource", self.issuer)
    }

    pub fn token_is_valid(&self, presented: &str) -> Result<bool> {
        self.store.token_is_valid(presented)
    }
}

fn secret_matches(expected: &str, presented: &str) -> bool {
    expected.len() == presented.len() && expected.as_bytes().ct_eq(presented.as_bytes()).into()
}

/// RFC 9728. Points at the authorization server, which happens to be this same
/// process.
pub async fn protected_resource(State(oauth): State<Arc<OAuth>>) -> Json<Value> {
    Json(json!({
        "resource": format!("{}/mcp", oauth.issuer),
        "authorization_servers": [oauth.issuer],
        "bearer_methods_supported": ["header"],
    }))
}

/// RFC 8414.
pub async fn authorization_server(State(oauth): State<Arc<OAuth>>) -> Json<Value> {
    Json(json!({
        "issuer": oauth.issuer,
        "authorization_endpoint": format!("{}/authorize", oauth.issuer),
        "token_endpoint": format!("{}/token", oauth.issuer),
        "registration_endpoint": format!("{}/register", oauth.issuer),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": ["mcp"],
    }))
}

/// RFC 7591. Clients register themselves; the consent screen is what actually
/// decides whether anyone gets a token, so registration itself is open.
pub async fn register(
    State(oauth): State<Arc<OAuth>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Response> {
    let redirect_uris: Vec<String> = body
        .get("redirect_uris")
        .and_then(Value::as_array)
        .map(|uris| {
            uris.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    if redirect_uris.is_empty() {
        return Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "at least one redirect_uri is required",
        ));
    }

    let client_id = Uuid::new_v4().to_string();
    oauth
        .store
        .register_client(&client_id, &redirect_uris)
        .map_err(|error| {
            tracing::error!(%error, "cannot record the registered client");
            oauth_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "cannot record the registered client",
            )
        })?;

    tracing::info!(%client_id, "registered an OAuth client");
    Ok(Json(json!({
        "client_id": client_id,
        "redirect_uris": redirect_uris,
        "token_endpoint_auth_method": "none",
        "grant_types": ["authorization_code"],
        "response_types": ["code"],
    })))
}

#[derive(Debug, Deserialize)]
pub struct AuthorizeParams {
    client_id: String,
    redirect_uri: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    code_challenge: String,
    #[serde(default)]
    code_challenge_method: String,
}

pub async fn authorize_form(
    State(oauth): State<Arc<OAuth>>,
    Query(params): Query<AuthorizeParams>,
) -> Result<Html<String>, Response> {
    validate_client(&oauth, &params.client_id, &params.redirect_uri)?;

    if params.code_challenge_method != "S256" || params.code_challenge.is_empty() {
        return Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "PKCE with S256 is required",
        ));
    }

    Ok(Html(consent_page(&params, None)))
}

#[derive(Debug, Deserialize)]
pub struct ConsentForm {
    client_id: String,
    redirect_uri: String,
    #[serde(default)]
    state: String,
    code_challenge: String,
    #[serde(default)]
    code_challenge_method: String,
    token: String,
}

pub async fn authorize_submit(
    State(oauth): State<Arc<OAuth>>,
    Form(form): Form<ConsentForm>,
) -> Result<Response, Response> {
    validate_client(&oauth, &form.client_id, &form.redirect_uri)?;

    if !secret_matches(&oauth.consent_secret, form.token.trim()) {
        tracing::warn!(client_id = %form.client_id, "consent refused: wrong token");
        let params = AuthorizeParams {
            client_id: form.client_id,
            redirect_uri: form.redirect_uri,
            state: form.state,
            code_challenge: form.code_challenge,
            code_challenge_method: form.code_challenge_method,
        };
        return Ok((
            StatusCode::UNAUTHORIZED,
            Html(consent_page(&params, Some("That token is not correct."))),
        )
            .into_response());
    }

    let code = Uuid::new_v4().to_string();
    oauth
        .store
        .store_code(
            &code,
            &PendingCode {
                client_id: form.client_id.clone(),
                redirect_uri: form.redirect_uri.clone(),
                code_challenge: form.code_challenge,
                expires_at: SystemTime::now() + CODE_TTL,
            },
        )
        .map_err(|error| {
            tracing::error!(%error, "cannot record the authorization code");
            oauth_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "cannot record the authorization code",
            )
        })?;

    let separator = if form.redirect_uri.contains('?') {
        '&'
    } else {
        '?'
    };
    let mut location = format!("{}{separator}code={code}", form.redirect_uri);
    if !form.state.is_empty() {
        location.push_str(&format!("&state={}", form.state));
    }

    tracing::info!(client_id = %form.client_id, "issued an authorization code");
    Ok((
        StatusCode::FOUND,
        [(header::LOCATION, location)],
        "authorized",
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
pub struct TokenRequest {
    grant_type: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    code_verifier: String,
}

pub async fn token(
    State(oauth): State<Arc<OAuth>>,
    Form(request): Form<TokenRequest>,
) -> Result<Json<Value>, Response> {
    if request.grant_type != "authorization_code" {
        return Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "only authorization_code is supported",
        ));
    }

    let pending = oauth
        .store
        // Removed on first use: an authorization code is single-use.
        .take_code(&request.code)
        .map_err(|error| {
            tracing::error!(%error, "cannot read the authorization code");
            oauth_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "cannot read the authorization code",
            )
        })?
        .ok_or_else(|| {
            oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "unknown or already-used authorization code",
            )
        })?;

    if pending.expires_at < SystemTime::now() {
        return Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "authorization code has expired",
        ));
    }
    if pending.redirect_uri != request.redirect_uri {
        return Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "redirect_uri does not match the authorization request",
        ));
    }

    let digest = Sha256::digest(request.code_verifier.as_bytes());
    if URL_SAFE_NO_PAD.encode(digest) != pending.code_challenge {
        tracing::warn!(client_id = %pending.client_id, "token exchange failed PKCE");
        return Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "code_verifier does not match code_challenge",
        ));
    }

    let access_token = Uuid::new_v4().simple().to_string();
    oauth
        .store
        .store_token(&access_token, SystemTime::now() + TOKEN_TTL)
        .map_err(|error| {
            tracing::error!(%error, "cannot record the issued token");
            oauth_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "cannot record the issued token",
            )
        })?;

    tracing::info!(client_id = %pending.client_id, "issued an access token");
    Ok(Json(json!({
        "access_token": access_token,
        "token_type": "Bearer",
        "expires_in": TOKEN_TTL.as_secs(),
        "scope": "mcp",
    })))
}

fn validate_client(oauth: &OAuth, client_id: &str, redirect_uri: &str) -> Result<(), Response> {
    let registered = oauth.store.redirect_uris(client_id).map_err(|error| {
        tracing::error!(%error, "cannot read the registered clients");
        oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "cannot read the registered clients",
        )
    })?;

    // An unregistered redirect_uri is how a stolen code gets delivered
    // somewhere else, so this is checked before anything is issued.
    match registered {
        Some(uris) if uris.iter().any(|u| u == redirect_uri) => Ok(()),
        Some(_) => Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "redirect_uri was not registered for this client",
        )),
        None => Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client",
            "unknown client_id",
        )),
    }
}

fn oauth_error(status: StatusCode, code: &str, description: &str) -> Response {
    (
        status,
        Json(json!({ "error": code, "error_description": description })),
    )
        .into_response()
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn consent_page(params: &AuthorizeParams, error: Option<&str>) -> String {
    let message = error
        .map(|e| format!(r#"<p class="error">{}</p>"#, escape(e)))
        .unwrap_or_default();

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Authorize local-mcp</title>
<style>
  :root {{ color-scheme: light dark; }}
  body {{ font: 16px/1.5 system-ui, sans-serif; display: grid; place-items: center;
         min-height: 100vh; margin: 0; padding: 1rem; }}
  form {{ width: 100%; max-width: 26rem; }}
  h1 {{ font-size: 1.25rem; margin-bottom: .25rem; }}
  p {{ margin: .25rem 0 1rem; opacity: .8; }}
  .error {{ color: #b00020; opacity: 1; }}
  input {{ width: 100%; padding: .6rem; font: inherit; box-sizing: border-box; }}
  button {{ margin-top: 1rem; width: 100%; padding: .6rem; font: inherit; cursor: pointer; }}
</style>
</head>
<body>
<form method="post" action="/authorize">
  <h1>Authorize access</h1>
  <p>A client is asking to read and edit files through local-mcp.
     Enter the server token to allow it.</p>
  {message}
  <input type="hidden" name="client_id" value="{client_id}">
  <input type="hidden" name="redirect_uri" value="{redirect_uri}">
  <input type="hidden" name="state" value="{state}">
  <input type="hidden" name="code_challenge" value="{challenge}">
  <input type="hidden" name="code_challenge_method" value="S256">
  <input type="password" name="token" placeholder="Server token" autofocus required>
  <button type="submit">Allow</button>
</form>
</body>
</html>"#,
        message = message,
        client_id = escape(&params.client_id),
        redirect_uri = escape(&params.redirect_uri),
        state = escape(&params.state),
        challenge = escape(&params.code_challenge),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_its_verifier() {
        let verifier = "a-verifier-long-enough-to-be-realistic-0123456789";
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
            challenge
        );
        assert_ne!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(b"a-different-verifier")),
            challenge
        );
    }

    #[test]
    fn consent_page_escapes_injected_markup() {
        let params = AuthorizeParams {
            client_id: "\"><script>alert(1)</script>".to_string(),
            redirect_uri: "https://example.com/cb".to_string(),
            state: String::new(),
            code_challenge: "abc".to_string(),
            code_challenge_method: "S256".to_string(),
        };
        let page = consent_page(&params, None);
        assert!(!page.contains("<script>alert(1)"));
        assert!(page.contains("&lt;script&gt;"));
    }

    #[test]
    fn secret_comparison_rejects_near_misses() {
        assert!(secret_matches("supersecrettoken", "supersecrettoken"));
        assert!(!secret_matches("supersecrettoken", "supersecrettoke"));
        assert!(!secret_matches("supersecrettoken", "Supersecrettoken"));
    }
}
