//! Administrator-owned registry and persistent MCP clients for child hosts.
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use rmcp::{
    ErrorData as McpError, Peer, RoleClient, ServiceExt,
    model::{CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock},
    service::{RunningService, ServiceError},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::root::Root;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Standalone,
    Client,
    Server,
}

impl Mode {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "standalone" => Ok(Self::Standalone),
            "client" => Ok(Self::Client),
            "server" => Ok(Self::Server),
            _ => bail!("LOCAL_MCP_MODE must be standalone, client or server"),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Standalone => "standalone",
            Self::Client => "client",
            Self::Server => "server",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryFile {
    connections: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    hostname: String,
    #[serde(default)]
    aliases: Vec<String>,
    url: String,
    token_file: PathBuf,
}

pub fn selector(raw: &str) -> Result<String> {
    let s = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    if s.is_empty()
        || !s
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-._:".contains(&c))
    {
        bail!("connection must be a registered hostname or IP alias");
    }
    Ok(s)
}

/// Check resolved paths, so a secret symlink cannot bypass root containment.
fn outside_root(path: &Path, root: &Root) -> Result<PathBuf> {
    let resolved = path
        .canonicalize()
        .context("cannot resolve registry or credential file")?;
    if resolved.starts_with(root.path()) {
        bail!("connection registry and credential files must be outside LOCAL_MCP_ROOT");
    }
    Ok(resolved)
}

pub struct Upstream {
    hostname: String,
    aliases: Vec<String>,
    url: String,
    token: String,
    client: reqwest::Client,
    service: Mutex<Option<RunningService<RoleClient, ()>>>,
}

#[derive(Default)]
pub struct Connections {
    children: Vec<Arc<Upstream>>,
}

impl std::fmt::Debug for Connections {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connections")
            .field("count", &self.children.len())
            .finish()
    }
}

impl Connections {
    pub fn load(path: &Path, root: &Root) -> Result<Self> {
        let path = outside_root(path, root)?;
        let registry: RegistryFile = serde_json::from_slice(&std::fs::read(&path)?)
            .context("invalid connection registry JSON")?;
        if registry.connections.is_empty() {
            bail!("central server needs at least one registered connection");
        }
        let mut seen = HashSet::new();
        let mut children = Vec::new();
        for entry in registry.connections {
            let hostname = selector(&entry.hostname)?;
            let aliases = entry
                .aliases
                .iter()
                .map(|s| selector(s))
                .collect::<Result<Vec<_>>>()?;
            for name in std::iter::once(&hostname).chain(aliases.iter()) {
                if !seen.insert(name.clone()) {
                    bail!("duplicate connection hostname or alias: {name}");
                }
            }
            let url = reqwest::Url::parse(&entry.url).context("invalid child MCP URL")?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || url.path() != "/local"
            {
                bail!(
                    "child URL must be http(s)://host[:port]/local without credentials, query or fragment"
                );
            }
            let secret_path = if entry.token_file.is_absolute() {
                entry.token_file
            } else {
                path.parent().unwrap().join(entry.token_file)
            };
            let token = std::fs::read_to_string(outside_root(&secret_path, root)?)?
                .trim()
                .to_owned();
            if token.len() < 16 || token.chars().any(|c| c.is_control()) {
                bail!("child token must be at least 16 characters without control characters");
            }
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .no_proxy()
                .connect_timeout(Duration::from_secs(3))
                .build()?;
            children.push(Arc::new(Upstream {
                hostname,
                aliases,
                url: url.to_string(),
                token,
                client,
                service: Mutex::new(None),
            }));
        }
        Ok(Self { children })
    }

    fn resolve(&self, raw: &str) -> Result<&Arc<Upstream>, McpError> {
        let name = selector(raw)
            .map_err(|_| McpError::invalid_params("invalid connection selector", None))?;
        self.children
            .iter()
            .find(|u| u.hostname == name || u.aliases.contains(&name))
            .ok_or_else(|| {
                McpError::invalid_params(
                    "unknown connection; use connections to list registered hosts",
                    None,
                )
            })
    }

    pub async fn call(
        &self,
        connection: &str,
        request: CallToolRequestParams,
    ) -> Result<CallToolResponse, McpError> {
        self.resolve(connection)?
            .call(request, Duration::from_secs(300))
            .await
    }

    pub async fn list(&self, allow_exec: bool) -> Value {
        // Probe independently: a dead host must not prevent healthy hosts appearing.
        let mut tasks = tokio::task::JoinSet::new();
        for child in &self.children {
            let child = child.clone();
            tasks.spawn(async move {
                let mut info = json!({"hostname": child.hostname, "aliases": child.aliases, "status": "unavailable"});
                if let Ok(CallToolResponse::Complete(result)) = child.call(
                    CallToolRequestParams::new("connections"), Duration::from_secs(5)
                ).await
                    && result.is_error != Some(true)
                        && let Some(v) = metadata(&result)
                        && let Some(first) = v["connections"].as_array().and_then(|a| a.first()) {
                        info["status"] = json!("connected");
                        info["root"] = first["root"].clone();
                        info["allow_exec"] = json!(allow_exec && first["allow_exec"].as_bool().unwrap_or(false));
                    }
                info
            });
        }
        let mut result = Vec::new();
        while let Some(Ok(info)) = tasks.join_next().await {
            result.push(info);
        }
        result.sort_by(|a, b| a["hostname"].as_str().cmp(&b["hostname"].as_str()));
        json!({"mode": "server", "connections": result})
    }
}

fn metadata(result: &CallToolResult) -> Option<Value> {
    result
        .content
        .first()?
        .as_text()
        .and_then(|t| serde_json::from_str(&t.text).ok())
}

impl Upstream {
    async fn peer(&self) -> Result<Peer<RoleClient>, McpError> {
        let mut slot = self.service.lock().await;
        if let Some(service) = slot.as_ref().filter(|s| !s.is_closed()) {
            return Ok(service.peer().clone());
        }
        let mut config = StreamableHttpClientTransportConfig::with_uri(self.url.clone())
            .auth_header(self.token.clone());
        // The SDK can replay an expired-session request; disable it for file writes.
        config.reinit_on_expired_session = false;
        let transport = StreamableHttpClientTransport::with_client(self.client.clone(), config);
        let service = tokio::time::timeout(Duration::from_secs(5), ().serve(transport))
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or_else(|| {
                McpError::internal_error("connection unavailable before tool submission", None)
            })?;
        let peer = service.peer().clone();
        // Reject a router as a child to avoid accidental recursive routing.
        let info = tokio::time::timeout(
            Duration::from_secs(5),
            peer.call_tool_once(CallToolRequestParams::new("connections")),
        )
        .await;
        let valid = matches!(info, Ok(Ok(CallToolResponse::Complete(ref result)))
            if result.is_error != Some(true) && metadata(result).is_some_and(|m|
                matches!(m["mode"].as_str(), Some("client" | "standalone"))
                    && m["connections"].as_array().is_some_and(|items| items.len() == 1
                        && items[0]["hostname"].as_str() == Some(self.hostname.as_str()))));
        if !valid {
            service.cancellation_token().cancel();
            return Err(McpError::internal_error(
                "child must expose local filesystem metadata matching the registered hostname",
                None,
            ));
        }
        *slot = Some(service);
        Ok(peer)
    }

    async fn call(
        &self,
        request: CallToolRequestParams,
        timeout: Duration,
    ) -> Result<CallToolResponse, McpError> {
        let peer = self.peer().await?;
        match tokio::time::timeout(timeout, peer.call_tool_once(request)).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(ServiceError::McpError(mut error))) => {
                error.message = error.message.replace(&self.token, "[redacted]").into();
                error.data = None;
                Err(error)
            },
            _ => {
                // Do not destroy other concurrent calls or automatically resubmit this call.
                // A later caller may reconnect when the transport reports itself closed.
                Err(McpError::internal_error(
                    "upstream response unconfirmed; operation may have executed. Do not automatically retry writes or commands.",
                    None,
                ))
            },
        }
    }
}

pub fn local_info(
    mode: Mode,
    hostname: &str,
    aliases: &[String],
    root: &Root,
    allow_exec: bool,
) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(
        json!({
            "mode": mode.name(), "connections": [{
                "hostname": hostname, "aliases": aliases, "root": root.path(),
                "status": "connected", "allow_exec": allow_exec
            }]
        })
        .to_string(),
    )])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Root, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root_path = dir.path().join("root");
        std::fs::create_dir(&root_path).unwrap();
        let root = Root::new(&root_path).unwrap();
        std::fs::write(dir.path().join("token"), "0123456789abcdef").unwrap();
        let path = dir.path().join("connections.json");
        (dir, root, path)
    }
    fn entry(name: &str, aliases: Value) -> Value {
        json!({"hostname": name, "aliases": aliases, "url": "http://100.64.0.1:8876/local", "token_file": "token"})
    }
    #[test]
    fn hostname_and_ip_alias_resolve_to_the_same_child() {
        let (_dir, root, path) = setup();
        std::fs::write(
            &path,
            json!({"connections":[entry("GPU", json!(["100.64.0.1"]))]}).to_string(),
        )
        .unwrap();
        let registry = Connections::load(&path, &root).unwrap();
        assert!(Arc::ptr_eq(
            registry.resolve("gpu").unwrap(),
            registry.resolve("100.64.0.1").unwrap()
        ));
        assert!(registry.resolve("missing").is_err());
    }
    #[test]
    fn duplicate_aliases_and_unsafe_urls_are_rejected() {
        let (_dir, root, path) = setup();
        for entries in [
            json!([entry("gpu", json!(["mac"])), entry("mac", json!([]))]),
            json!([{"hostname":"gpu","url":"http://user:pass@example.com/local","token_file":"token"}]),
            json!([{"hostname":"gpu","url":"http://example.com/local?token=secret","token_file":"token"}]),
        ] {
            std::fs::write(&path, json!({"connections":entries}).to_string()).unwrap();
            assert!(Connections::load(&path, &root).is_err());
        }
    }
    #[test]
    fn secrets_inside_the_root_are_rejected_even_through_symlinks() {
        let (dir, root, path) = setup();
        std::fs::write(root.path().join("secret"), "0123456789abcdef").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.path().join("secret"), dir.path().join("link")).unwrap();
        let token_file = if cfg!(unix) { "link" } else { "root/secret" };
        let value = json!({"connections":[{"hostname":"gpu","url":"http://gpu/local","token_file":token_file}]});
        std::fs::write(&path, value.to_string()).unwrap();
        assert!(Connections::load(&path, &root).is_err());
        std::fs::write(root.path().join("registry.json"), value.to_string()).unwrap();
        assert!(Connections::load(&root.path().join("registry.json"), &root).is_err());
    }
}
