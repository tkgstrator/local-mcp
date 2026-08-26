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
    StreamableHttpService, StreamableHttpServerConfig, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::{config::Config, server::LocalMcp};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "local_mcp=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Arc::new(Config::from_env()?);
    let cancel = CancellationToken::new();

    let service = {
        let config = config.clone();
        StreamableHttpService::new(
            move || Ok(LocalMcp::new(config.clone())),
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default().with_cancellation_token(cancel.child_token()),
        )
    };

    let app = Router::new()
        .merge(
            Router::new()
                .nest_service("/mcp", service)
                .layer(middleware::from_fn_with_state(config.clone(), auth::guard)),
        )
        .route("/healthz", get(|| async { "ok" }));

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
