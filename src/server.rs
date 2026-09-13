use std::sync::Arc;

use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use uuid::Uuid;

use crate::{config::Config, exec_ops::Jobs, fs_ops};

const EXEC_TOOLS: [&str; 4] = ["execute", "start_command", "poll_job", "stop_job"];

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
    Uuid::parse_str(raw).map_err(|_| McpError::invalid_params(format!("bad job id: {raw}"), None))
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
}
