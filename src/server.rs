use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
        Implementation, JsonObject, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
        ResultType, ServerCapabilities, ServerInfo,
    },
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use serde_json::Value;
use uuid::Uuid;

use crate::{config::Config, exec_ops::Jobs, fs_ops};

const EXEC_TOOLS: [&str; 4] = ["execute", "start_command", "poll_job", "stop_job"];

/// How long a client may reuse the tool list before asking for it again.
///
/// The SDK answers `tools/list` with `ttlMs: 0`, which tells a client on
/// 2026-07-28 that the list is never reusable. Sessions are gone in that
/// version, so that number is the only thing standing between a conversation
/// and a fresh round trip through the tunnel every time it might reach for a
/// tool — and a single failed round trip is enough for the tools to disappear
/// from a connector that still reads as connected. The list only changes when
/// this binary does, so there is nothing to gain from re-fetching it that
/// often.
const TOOL_LIST_TTL: Duration = Duration::from_secs(60 * 60);

/// How long the tool list may be reused, and by whom, for a client that
/// negotiated a version where those fields exist at all. `None` for anyone
/// older: the fields arrived with 2026-07-28 (SEP-2549), and a client that
/// predates them has no way to read them.
fn cache_hints(version: Option<ProtocolVersion>) -> Option<(u64, CacheScope)> {
    (version? >= ProtocolVersion::V_2026_07_28).then_some((
        TOOL_LIST_TTL.as_millis() as u64,
        // Public rather than private: the list is the same for everyone who
        // gets past the token, so no part of it belongs to one caller rather
        // than another.
        CacheScope::Public,
    ))
}

/// Longest string argument that goes into the log as itself. A path or a
/// command is worth seeing in full; the contents of a file being written are
/// not, and would push everything else off the line.
const ARGUMENT_ELISION: usize = 160;

/// Longest error detail carried into the log from a tool that reported its own
/// failure.
const DETAIL_ELISION: usize = 240;

fn elide(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((cut, _)) => format!("{}... [{} bytes]", &text[..cut], text.len()),
        None => text.to_owned(),
    }
}

/// The arguments of a call, rendered for one log line.
fn arguments(arguments: Option<&JsonObject>) -> String {
    let Some(arguments) = arguments else {
        return "{}".to_owned();
    };
    let shown: JsonObject = arguments
        .iter()
        .map(|(key, value)| {
            let value = match value {
                Value::String(text) => Value::String(elide(text, ARGUMENT_ELISION)),
                other => other.clone(),
            };
            (key.clone(), value)
        })
        .collect();
    serde_json::to_string(&shown).unwrap_or_else(|_| "<unprintable>".to_owned())
}

/// What a tool said when it reported a failure of its own.
fn detail(result: &CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|block| block.as_text())
        .map(|text| elide(&text.text, DETAIL_ELISION))
        .unwrap_or_else(|| "<no content>".to_owned())
}

#[derive(Clone)]
pub struct LocalMcp {
    config: Arc<Config>,
    jobs: Jobs,
    tool_router: ToolRouter<LocalMcp>,
}

fn text(body: impl Into<String>) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::text(body)]))
}

fn failed(error: anyhow::Error) -> McpError {
    McpError::invalid_params(format!("{error:#}"), None)
}

fn job_id(raw: &str) -> Result<Uuid, McpError> {
    Uuid::parse_str(raw).map_err(|_| {
        McpError::invalid_params(
            format!(
                "not a job id: {raw} (execute and start_command hand one back when they leave \
                 something running)"
            ),
            None,
        )
    })
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReadFileArgs {
    /// Path relative to the sandbox root.
    pub path: String,
    /// Zero-based line number to start from.
    pub offset: Option<u32>,
    /// Maximum number of lines to return.
    pub limit: Option<u32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct WriteFileArgs {
    /// Path relative to the sandbox root. Parent directories are created.
    pub path: String,
    /// Full contents to write, replacing anything already there.
    pub content: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct EditFileArgs {
    /// Path relative to the sandbox root.
    pub path: String,
    /// Exact text to replace. Must occur exactly once in the file.
    pub old_text: String,
    /// Replacement text.
    pub new_text: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ListDirArgs {
    /// Directory relative to the sandbox root. Defaults to the root.
    pub path: Option<String>,
    /// How deep to descend. Defaults to 1.
    pub depth: Option<u32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// Rust regular expression to match against each line.
    pub pattern: String,
    /// Directory to search under. Defaults to the sandbox root.
    pub path: Option<String>,
    /// Stop after this many matching lines. Defaults to 200.
    pub max_results: Option<u32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CommandArgs {
    /// Shell command, run with `sh -c` from the sandbox root.
    pub command: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct JobArgs {
    /// Job id returned by `execute` or `start_command`.
    pub job_id: String,
}

#[tool_router]
impl LocalMcp {
    pub fn new(config: Arc<Config>, jobs: Jobs) -> Self {
        let mut tool_router = Self::tool_router();
        if !config.allow_exec {
            for name in EXEC_TOOLS {
                tool_router.remove_route(name);
            }
        }
        Self {
            config,
            jobs,
            tool_router,
        }
    }

    #[tool(description = "Read a text file, returned with line numbers.")]
    async fn read_file(
        &self,
        Parameters(args): Parameters<ReadFileArgs>,
    ) -> Result<CallToolResult, McpError> {
        fs_ops::read_file(
            &self.config.root,
            &args.path,
            args.offset,
            args.limit,
            self.config.max_output,
        )
        .await
        .map_err(failed)
        .and_then(text)
    }

    #[tool(description = "Create a file or replace its entire contents.")]
    async fn write_file(
        &self,
        Parameters(args): Parameters<WriteFileArgs>,
    ) -> Result<CallToolResult, McpError> {
        fs_ops::write_file(&self.config.root, &args.path, &args.content)
            .await
            .map_err(failed)
            .and_then(text)
    }

    #[tool(
        description = "Replace one exact occurrence of old_text with new_text. Fails if old_text is missing or appears more than once."
    )]
    async fn edit_file(
        &self,
        Parameters(args): Parameters<EditFileArgs>,
    ) -> Result<CallToolResult, McpError> {
        fs_ops::edit_file(
            &self.config.root,
            &args.path,
            &args.old_text,
            &args.new_text,
        )
        .await
        .map_err(failed)
        .and_then(text)
    }

    #[tool(description = "List directory entries with sizes.")]
    async fn list_dir(
        &self,
        Parameters(args): Parameters<ListDirArgs>,
    ) -> Result<CallToolResult, McpError> {
        fs_ops::list_dir(
            &self.config.root,
            args.path.as_deref(),
            args.depth,
            self.config.max_output,
        )
        .await
        .map_err(failed)
        .and_then(text)
    }

    #[tool(description = "Search file contents by regular expression, honouring .gitignore.")]
    async fn search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        fs_ops::search(
            &self.config.root,
            &args.pattern,
            args.path.as_deref(),
            args.max_results,
            self.config.max_output,
        )
        .await
        .map_err(failed)
        .and_then(text)
    }

    #[tool(
        description = "Run a shell command and wait for it. If it is still running after the timeout, returns a job_id to poll instead."
    )]
    async fn execute(
        &self,
        Parameters(args): Parameters<CommandArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = self
            .jobs
            .start(&self.config.root, &args.command, self.config.max_output)
            .map_err(failed)?;

        match self
            .jobs
            .wait(id, self.config.command_timeout)
            .await
            .map_err(failed)?
        {
            Some(finished) => text(format!("{}\n\n{}", finished.status, finished.output)),
            None => text(format!(
                "still running after {}s; poll with job_id {id}",
                self.config.command_timeout.as_secs()
            )),
        }
    }

    #[tool(description = "Start a shell command in the background and return its job_id.")]
    async fn start_command(
        &self,
        Parameters(args): Parameters<CommandArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = self
            .jobs
            .start(&self.config.root, &args.command, self.config.max_output)
            .map_err(failed)?;
        text(format!("started job {id}"))
    }

    #[tool(description = "Check a background job's status and output so far.")]
    async fn poll_job(
        &self,
        Parameters(args): Parameters<JobArgs>,
    ) -> Result<CallToolResult, McpError> {
        let (command, snapshot) = self.jobs.poll(job_id(&args.job_id)?).map_err(failed)?;
        text(format!(
            "{command}\n{}\n\n{}",
            snapshot.status, snapshot.output
        ))
    }

    #[tool(description = "Kill a background job and everything it spawned.")]
    async fn stop_job(
        &self,
        Parameters(args): Parameters<JobArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.jobs
            .stop(job_id(&args.job_id)?)
            .map_err(failed)
            .and_then(text)
    }
}

// Without an explicit router the macro rebuilds `Self::tool_router()` per call,
// which would silently resurrect the tools removed in `new`.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for LocalMcp {
    /// Same dispatch the macro would generate, with a line on either side of it.
    ///
    /// Both lines earn their place, not just the second one: a call that starts
    /// and never finishes is exactly the shape a client that gave up waiting
    /// leaves behind, and without the first line there is nothing in the log to
    /// say the call ever arrived.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let tool = request.name.clone();
        tracing::info!(%tool, arguments = %arguments(request.arguments.as_ref()), "tool call");

        let started = Instant::now();
        let outcome = self
            .tool_router
            .call(ToolCallContext::new(self, request, context))
            .await;
        let elapsed_ms = started.elapsed().as_millis();

        match &outcome {
            Err(error) => tracing::warn!(%tool, elapsed_ms, %error, "tool call failed"),
            // A failure the SDK folds into the result rather than returning as
            // an error still needs to read as a failure here.
            Ok(CallToolResponse::Complete(result)) if result.is_error == Some(true) => {
                tracing::warn!(%tool, elapsed_ms, detail = %detail(result), "tool call failed");
            },
            Ok(_) => tracing::info!(%tool, elapsed_ms, "tool call done"),
        }

        outcome
    }

    /// Same list the macro would generate, with a cache lifetime on it.
    ///
    /// Defining it here is what keeps `tool_handler` from generating its own:
    /// the macro only fills in the methods the impl block is missing.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let (ttl_ms, cache_scope) = match cache_hints(context.protocol_version()) {
            Some((ttl_ms, scope)) => (Some(ttl_ms), Some(scope)),
            None => (None, None),
        };

        Ok(ListToolsResult {
            result_type: Some(ResultType::COMPLETE),
            tools: self.tool_router.list_all(),
            meta: None,
            next_cursor: None,
            ttl_ms,
            cache_scope,
        })
    }

    fn get_info(&self) -> ServerInfo {
        let root = self.config.root.path().display();
        let shell = if self.config.allow_exec {
            " Long-running commands return a job_id; poll it with poll_job and end it with stop_job."
        } else {
            " Shell access is disabled on this instance."
        };

        // `from_build_env` reports the SDK crate, not this one.
        let mut implementation = Implementation::from_build_env();
        implementation.name = env!("CARGO_PKG_NAME").to_string();
        implementation.version = env!("CARGO_PKG_VERSION").to_string();
        implementation.title = Some("NEVER KNOWS BEST".to_string());
        implementation.description = Some(env!("CARGO_PKG_DESCRIPTION").to_string());
        implementation.website_url = Some(env!("CARGO_PKG_REPOSITORY").to_string());

        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(implementation)
            .with_instructions(format!(
                "File tools are confined to {root}; their paths are relative to that root \
                 and cannot escape it.{shell}"
            ))
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, path::PathBuf, time::Duration};

    use super::*;
    use crate::root::Root;

    fn config(root: Root) -> Arc<Config> {
        Arc::new(Config {
            root,
            token: "0123456789abcdef".to_string(),
            bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            allowed_hosts: Vec::new(),
            public_url: None,
            state_db: PathBuf::from("/var/lib/local-mcp/oauth.db"),
            max_output: 4096,
            command_timeout: Duration::from_secs(5),
            allow_exec: true,
            job_retention: Duration::from_secs(3600),
            session_keep_alive: None,
            session_retention: Duration::from_secs(60 * 60 * 24 * 30),
        })
    }

    fn body(result: &CallToolResult) -> String {
        result
            .content
            .first()
            .and_then(|block| block.as_text())
            .map(|text| text.text.clone())
            .expect("a tool that succeeded returns text")
    }

    /// The regression this guards: `main` calls `LocalMcp::new` once per session,
    /// so a handler that built its own `Jobs` gave every session an empty table.
    /// A client whose session expires opens a new one and comes back holding a
    /// job id from the old one, and that id has to still resolve.
    #[tokio::test]
    async fn a_job_outlives_the_session_it_was_started_in() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(Root::new(dir.path()).unwrap());
        let jobs = Jobs::new(config.job_retention);

        let first = LocalMcp::new(config.clone(), jobs.clone());
        let second = LocalMcp::new(config.clone(), jobs.clone());

        let started = first
            .start_command(Parameters(CommandArgs {
                command: "echo across-sessions".to_string(),
            }))
            .await
            .unwrap();
        let id = body(&started)
            .trim()
            .rsplit(' ')
            .next()
            .expect("start_command names the job id last")
            .to_string();

        // `wait` on the shared table, so the assertion does not race the shell.
        jobs.wait(job_id(&id).unwrap(), Duration::from_secs(10))
            .await
            .unwrap()
            .expect("echo finishes well inside the timeout");

        let polled = second
            .poll_job(Parameters(JobArgs { job_id: id }))
            .await
            .expect("the second session knows the job the first one started");
        let report = body(&polled);
        assert!(report.contains("exited with code 0"), "{report}");
        assert!(report.contains("across-sessions"), "{report}");
    }

    /// `allow_exec = false` has to remove the routes, not merely hide them.
    #[tokio::test]
    async fn disabling_exec_removes_the_shell_tools() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = (*config(Root::new(dir.path()).unwrap())).clone();
        settings.allow_exec = false;
        let settings = Arc::new(settings);

        let server = LocalMcp::new(settings.clone(), Jobs::new(settings.job_retention));
        let offered: Vec<_> = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();

        for name in EXEC_TOOLS {
            assert!(!offered.contains(&name.to_string()), "{name} still offered");
        }
        assert!(offered.contains(&"read_file".to_string()));
    }

    /// The regression this guards: the SDK answers `tools/list` with `ttlMs: 0`,
    /// and 2026-07-28 has no sessions to fall back on, so that number leaves a
    /// conversation re-fetching the list through the tunnel every time it might
    /// reach for a tool. One fetch that does not come back is then enough for
    /// the tools to disappear from a connector that still reads as connected.
    #[test]
    fn the_tool_list_is_worth_caching_for_clients_that_can_cache_it() {
        let (ttl_ms, scope) = cache_hints(Some(ProtocolVersion::V_2026_07_28))
            .expect("2026-07-28 is where the fields came from");

        assert!(ttl_ms > 0, "ttlMs: 0 asks the client not to cache at all");
        assert_eq!(scope, CacheScope::Public);
    }

    /// Older clients have to be answered without the fields rather than with
    /// them set to something harmless: they do not exist in those versions.
    #[test]
    fn an_older_client_is_told_nothing_about_caching() {
        assert_eq!(cache_hints(Some(ProtocolVersion::V_2025_06_18)), None);
        assert_eq!(cache_hints(None), None);
    }
}
