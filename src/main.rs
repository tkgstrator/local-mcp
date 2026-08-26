mod auth;
mod config;
mod exec_ops;
mod fs_ops;
mod root;
mod server;

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{Router, middleware, routing::get};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::{config::Config, server::LocalMcp};

#[tokio::main]
async fn main() -> Result<()> {
    // LOCAL_MCP_LOG first so the knob matches the other settings; RUST_LOG still
    // works for anyone who reaches for it out of habit.
    let filter = std::env::var("LOCAL_MCP_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        // rmcp included: the SDK refuses some requests before they reach this
        // crate, and without its warnings those look like unexplained 403s.
        .unwrap_or_else(|_| "local_mcp=info,tower_http=info,rmcp=warn".to_string());

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(filter))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Arc::new(Config::from_env()?);
    let cancel = CancellationToken::new();

    let service = {
        // Read before the closure takes ownership of the Arc.
        let allowed_hosts = config.allowed_hosts.clone();
        let config = config.clone();
        StreamableHttpService::new(
            move || Ok(LocalMcp::new(config.clone())),
            LocalSessionManager::default().into(),
            // The SDK allows only localhost by default, so a request arriving
            // under a real hostname is refused before it reaches any tool.
            StreamableHttpServerConfig::default()
                .with_cancellation_token(cancel.child_token())
                .with_allowed_hosts(allowed_hosts),
        )
    };

    // `route_layer`, not `layer`: the guard must apply to matched routes only.
    // With `layer` it also wraps the 404 fallback, so every unknown path answers
    // 401 — including the /.well-known/* probes a client makes before it
    // connects. A 401 there reads as "this server wants OAuth", and clients that
    // would otherwise have sent the bearer token go looking for authorization
    // metadata that does not exist.
    let app = Router::new()
        .merge(
            Router::new()
                .nest_service("/mcp", service)
                .route_layer(middleware::from_fn_with_state(config.clone(), auth::guard)),
        )
        .route("/healthz", get(|| async { "ok" }))
        // At INFO because the default filter has to show these: a client whose
        // probe is being refused otherwise looks exactly like a client that
        // never connected, and there is no other way to tell the two apart.
        .layer(
            TraceLayer::new_for_http()
                // The span carries the method and path; without raising it too,
                // the log says a request finished but not which one.
                .make_span_with(DefaultMakeSpan::new().level(tracing::Level::INFO))
                .on_response(DefaultOnResponse::new().level(tracing::Level::INFO)),
        );

    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("cannot bind {}", config.bind))?;

    tracing::info!(
        root = %config.root.path().display(),
        bind = %config.bind,
        origins = config.allowed_origins.len(),
        "local-mcp listening on /mcp"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            cancel.cancel();
        })
        .await
        .context("server failed")
}
