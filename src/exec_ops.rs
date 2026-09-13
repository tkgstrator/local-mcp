use std::{
    collections::HashMap,
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
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
    /// When the process exited, so `reap` can tell a job that merely finished
    /// from one whose output nobody is coming back for.
    finished_at: Mutex<Option<Instant>>,
    pid: Option<u32>,
}

/// Shared by every session rather than built per connection: a job id handed
/// out in one turn has to still resolve after the client reconnects, and a
/// client whose session expires opens a new one without being asked to.
#[derive(Clone)]
pub struct Jobs {
    inner: Arc<Mutex<HashMap<Uuid, Arc<JobState>>>>,
    retention: Duration,
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
    pub fn new(retention: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            retention,
        }
    }

    /// Drop jobs that finished longer than `retention` ago. Called from `start`
    /// rather than from a timer: the table only grows when a job is started, so
    /// that is the only moment it can need trimming. A job still running is
    /// never reaped, however long it has been going.
    fn reap(&self) {
        let Ok(mut jobs) = self.inner.lock() else {
            return;
        };
        jobs.retain(|_, state| {
            let finished_at = state.finished_at.lock().ok().and_then(|at| *at);
            finished_at.is_none_or(|at| at.elapsed() < self.retention)
        });
    }

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
        self.reap();

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
            finished_at: Mutex::new(None),
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
            if let Ok(mut finished_at) = waiter.finished_at.lock() {
                *finished_at = Some(Instant::now());
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

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: usize = 4096;
    /// Long enough that nothing is reaped while a test is looking at it.
    const FOREVER: Duration = Duration::from_secs(3600);

    fn sandbox() -> (tempfile::TempDir, Root) {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::new(dir.path()).unwrap();
        (dir, root)
    }

    /// The regression this module exists for: `LocalMcp` is rebuilt per session,
    /// so `Jobs` is cloned rather than created there. A clone that did not share
    /// the table would answer "no such job" for every id issued before the
    /// client reconnected.
    #[tokio::test]
    async fn a_job_started_on_one_clone_resolves_through_another() {
        let (_dir, root) = sandbox();
        let first = Jobs::new(FOREVER);
        let second = first.clone();

        let id = first.start(&root, "echo shared", MAX).unwrap();
        let finished = second.wait(id, FOREVER).await.unwrap().unwrap();

        assert_eq!(finished.status, Status::Exited(0));
        assert_eq!(finished.output.trim(), "shared");
        assert_eq!(second.poll(id).unwrap().0, "echo shared");
    }

    #[tokio::test]
    async fn a_finished_job_is_dropped_once_its_retention_has_passed() {
        let (_dir, root) = sandbox();
        let jobs = Jobs::new(Duration::ZERO);

        let stale = jobs.start(&root, "true", MAX).unwrap();
        jobs.wait(stale, FOREVER).await.unwrap().unwrap();

        // Reaping happens on `start`, so it takes a second job to trigger it.
        jobs.start(&root, "true", MAX).unwrap();

        assert!(
            jobs.poll(stale).is_err(),
            "a job past its retention should be gone"
        );
    }

    #[tokio::test]
    async fn a_running_job_is_never_reaped() {
        let (_dir, root) = sandbox();
        let jobs = Jobs::new(Duration::ZERO);

        let running = jobs.start(&root, "sleep 30", MAX).unwrap();
        jobs.start(&root, "true", MAX).unwrap();

        let (command, snapshot) = jobs.poll(running).expect("still running, so still known");
        assert_eq!(command, "sleep 30");
        assert_eq!(snapshot.status, Status::Running);

        jobs.stop(running).unwrap();
    }

    #[tokio::test]
    async fn stopping_a_job_takes_its_children_with_it() {
        let (_dir, root) = sandbox();
        let jobs = Jobs::new(FOREVER);

        let id = jobs.start(&root, "sleep 30 & sleep 30", MAX).unwrap();
        jobs.stop(id).unwrap();

        let finished = jobs.wait(id, FOREVER).await.unwrap().unwrap();
        assert_eq!(finished.status, Status::Signalled);
        assert!(jobs.stop(id).is_err(), "a dead job cannot be stopped twice");
    }
}
