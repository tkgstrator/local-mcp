use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};

use crate::root::Root;

#[derive(Clone, Debug)]
pub struct Config {
    pub root: Root,
    pub token: String,
    pub bind: SocketAddr,
    pub allowed_origins: Vec<String>,
    pub allowed_hosts: Vec<String>,
    /// Public origin this server is reached at, e.g. `https://mcp.example.com`.
    /// OAuth metadata has to advertise absolute URLs, so the flow is only
    /// offered when this is known.
    pub public_url: Option<String>,
    /// Where issued tokens and registered clients are kept between restarts.
    /// Deliberately outside `root`: the file tools can read anything under
    /// that, and this file is a set of live credentials.
    pub state_db: PathBuf,
    pub max_output: usize,
    pub command_timeout: Duration,
    pub allow_exec: bool,
}

fn var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let root_dir =
            PathBuf::from(var("LOCAL_MCP_ROOT").unwrap_or_else(|| "/workspace".to_string()));
        let root = Root::new(&root_dir)
            .with_context(|| format!("LOCAL_MCP_ROOT is unusable: {}", root_dir.display()))?;

        // An unauthenticated instance of this server hands anyone who finds the
        // URL a shell inside the container, so refuse to start without a token.
        let Some(token) = var("LOCAL_MCP_TOKEN") else {
            bail!("LOCAL_MCP_TOKEN is required; refusing to start without authentication");
        };
        if token.len() < 16 {
            bail!("LOCAL_MCP_TOKEN must be at least 16 characters");
        }

        let bind = var("LOCAL_MCP_BIND")
            .unwrap_or_else(|| "0.0.0.0:8080".to_string())
            .parse()
            .context("LOCAL_MCP_BIND must be an address like 0.0.0.0:8080")?;

        let allowed_origins = var("LOCAL_MCP_ALLOWED_ORIGINS")
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        // The SDK defaults this to localhost only, which refuses every request
        // that arrives under a real hostname. Empty disables the check; the
        // bearer token is what actually guards the endpoint.
        let allowed_hosts = var("LOCAL_MCP_ALLOWED_HOSTS")
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        let public_url = var("LOCAL_MCP_PUBLIC_URL").map(|u| u.trim_end_matches('/').to_string());

        let state_db = PathBuf::from(
            var("LOCAL_MCP_STATE_DB").unwrap_or_else(|| "/var/lib/local-mcp/oauth.db".to_string()),
        );
        if state_db.starts_with(root.path()) {
            bail!(
                "LOCAL_MCP_STATE_DB ({}) is inside LOCAL_MCP_ROOT, which would expose issued \
                 tokens to the file tools",
                state_db.display()
            );
        }

        let max_output = var("LOCAL_MCP_MAX_OUTPUT")
            .map(|v| v.parse::<usize>())
            .transpose()
            .context("LOCAL_MCP_MAX_OUTPUT must be a byte count")?
            .unwrap_or(1_048_576);

        let command_timeout = var("LOCAL_MCP_COMMAND_TIMEOUT")
            .map(|v| v.parse::<u64>())
            .transpose()
            .context("LOCAL_MCP_COMMAND_TIMEOUT must be seconds")?
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(30));

        // Shell access cannot be confined to the root the way file tools are,
        // so it is worth being able to turn off without redeploying.
        let allow_exec = !matches!(
            var("LOCAL_MCP_ALLOW_EXEC").as_deref(),
            Some("false" | "0" | "no")
        );

        Ok(Self {
            root,
            token,
            bind,
            allowed_origins,
            allowed_hosts,
            public_url,
            state_db,
            max_output,
            command_timeout,
            allow_exec,
        })
    }
}
