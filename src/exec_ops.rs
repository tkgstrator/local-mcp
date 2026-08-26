use std::{
    collections::HashMap,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tokio::{io::AsyncReadExt, process::Command, sync::watch};
use uuid::Uuid;

use crate::root::Root;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Running,
    Exited(i32),
    Signalled,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => write!(f, "running"),
            Self::Exited(code) => write!(f, "exited with code {code}"),
            Self::Signalled => write!(f, "terminated by signal"),
        }
    }
}

struct JobState {
    command: String,
    output: Mutex<Vec<u8>>,
    status: Mutex<Status>,
    /// Flips to true exactly once, so a waiter that arrives late still sees it.
    done: watch::Sender<bool>,
    pid: Option<u32>,
}

#[derive(Clone, Default)]
pub struct Jobs {
    inner: Arc<Mutex<HashMap<Uuid, Arc<JobState>>>>,
}

pub struct Finished {
    pub status: Status,
    pub output: String,
}

async fn pump<R: AsyncReadExt + Unpin>(mut reader: R, state: Arc<JobState>, max_output: usize) {
    let mut chunk = [0u8; 8192];
    while let Ok(read) = reader.read(&mut chunk).await {
        if read == 0 {
            break;
        }
        let Ok(mut output) = state.output.lock() else {
            break;
        };
        if output.len() >= max_output {
            continue;
        }
        let room = max_output - output.len();
        output.extend_from_slice(&chunk[..read.min(room)]);
    }
}

impl Jobs {
    fn get(&self, id: Uuid) -> Result<Arc<JobState>> {
        self.inner
            .lock()
            .ok()
            .and_then(|jobs| jobs.get(&id).cloned())
            .with_context(|| format!("no such job: {id}"))
    }

    fn snapshot(state: &JobState) -> Finished {
        let output = state
            .output
            .lock()
            .map(|o| String::from_utf8_lossy(&o).into_owned())
            .unwrap_or_default();
        let status = state.status.lock().map(|s| *s).unwrap_or(Status::Running);
        Finished { status, output }
    }

    /// Launch a command in its own process group so that stopping it takes the
    /// whole tree down rather than orphaning grandchildren.
    pub fn start(&self, root: &Root, command: &str, max_output: usize) -> Result<Uuid> {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("cannot start command: {command}"))?;

        let (done, _) = watch::channel(false);
        let state = Arc::new(JobState {
            command: command.to_string(),
            output: Mutex::new(Vec::new()),
            status: Mutex::new(Status::Running),
            done,
            pid: child.id(),
        });

        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(pump(stdout, state.clone(), max_output));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(pump(stderr, state.clone(), max_output));
        }

        let id = Uuid::new_v4();
        let waiter = state.clone();
        tokio::spawn(async move {
            let exit = child.wait().await;
            if let Ok(mut status) = waiter.status.lock() {
                *status = match exit.map(|e| e.code()) {
                    Ok(Some(code)) => Status::Exited(code),
                    Ok(None) => Status::Signalled,
                    Err(_) => Status::Signalled,
                };
            }
            let _ = waiter.done.send(true);
        });

        self.inner
            .lock()
            .map_err(|_| anyhow::anyhow!("job table is poisoned"))?
            .insert(id, state);
        Ok(id)
    }

    /// Wait for a job, giving up after `timeout` so a long command can be
    /// handed back to the caller as a job id instead of blocking the request.
    pub async fn wait(&self, id: Uuid, timeout: Duration) -> Result<Option<Finished>> {
        let state = self.get(id)?;
        let mut done = state.done.subscribe();
        // The borrow returned by `wait_for` is not Send, so it must not survive
        // into the sleep below.
        let finished = tokio::time::timeout(timeout, async {
            let _ = done.wait_for(|finished| *finished).await;
        })
        .await
        .is_ok();

        if !finished {
            return Ok(None);
        }
        // Give the output pumps a moment to drain the final chunk.
        tokio::time::sleep(Duration::from_millis(20)).await;
        Ok(Some(Self::snapshot(&state)))
    }

    pub fn poll(&self, id: Uuid) -> Result<(String, Finished)> {
        let state = self.get(id)?;
        Ok((state.command.clone(), Self::snapshot(&state)))
    }

    pub fn stop(&self, id: Uuid) -> Result<String> {
        let state = self.get(id)?;
        let running = state
            .status
            .lock()
            .map(|s| *s == Status::Running)
            .unwrap_or(false);
        if !running {
            bail!("job {id} is no longer running");
        }
        let Some(pid) = state.pid else {
            bail!("job {id} has no pid");
        };
        // Negative pid targets the process group created by `process_group(0)`.
        let killed = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        if killed != 0 {
            bail!("failed to signal job {id}");
        }
        Ok(format!("stopped job {id}"))
    }
}
