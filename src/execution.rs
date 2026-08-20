use crate::{
    runtime::{RunningExec, Sandbox},
    tasks::{Event, ExecRequest, TaskSnapshot, TaskStore},
};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tracing::{debug, info, warn};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    #[error("argv must not be empty")]
    EmptyArgv,
}

pub struct ExecutionService {
    sandbox: Arc<Sandbox>,
    tasks: TaskStore,
    max_timeout: Duration,
}

impl ExecutionService {
    pub fn new(sandbox: Arc<Sandbox>, max_timeout: Duration) -> Self {
        let tasks = TaskStore::new();
        tasks.start_cleanup();
        Self {
            sandbox,
            tasks,
            max_timeout,
        }
    }

    pub async fn submit(&self, request: ExecRequest) -> Result<Uuid, SubmitError> {
        if request.argv.is_empty() {
            return Err(SubmitError::EmptyArgv);
        }

        let timeout = Duration::from_secs(
            request
                .timeout_seconds
                .unwrap_or(300)
                .min(self.max_timeout.as_secs()),
        );
        let (id, _, lease) = self.tasks.create().await;
        let tasks = self.tasks.clone();
        let sandbox = self.sandbox.clone();
        tokio::spawn(async move {
            let _lease = lease;
            run_task(tasks, sandbox, id, request, timeout).await;
        });
        Ok(id)
    }

    pub async fn snapshot(&self, id: Uuid) -> Option<TaskSnapshot> {
        self.tasks.snapshot(id).await
    }

    pub async fn subscribe(
        &self,
        id: Uuid,
    ) -> Option<(Vec<Event>, tokio::sync::broadcast::Receiver<Event>)> {
        self.tasks.subscribe(id).await
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        info!("waiting for active execution tasks");
        self.tasks.wait_for_idle().await;
        self.tasks.shutdown_cleanup().await;
        self.sandbox.shutdown().await
    }
}

async fn run_task(
    tasks: TaskStore,
    sandbox: Arc<Sandbox>,
    id: Uuid,
    request: ExecRequest,
    timeout: Duration,
) {
    info!(
        task_id = %id,
        program = %request.argv.first().map(String::as_str).unwrap_or(""),
        argc = request.argv.len(),
        timeout_seconds = timeout.as_secs(),
        "execution task started"
    );
    tasks.publish(id, Event::Started).await;

    let running = match sandbox.exec(&request).await {
        Ok(running) => running,
        Err(error) => {
            warn!(task_id = %id, error = %error, "failed to start execution task");
            tasks.publish(id, Event::Failed(error.to_string())).await;
            return;
        }
    };
    let RunningExec {
        mut child,
        stdin,
        stdout,
        stderr,
    } = running;

    if let Some(mut pipe) = stdin {
        if let Some(input) = request.stdin {
            if let Err(error) = pipe.write_all(input.as_bytes()).await {
                debug!(task_id = %id, error = %error, "failed to write task stdin");
            }
        }
    }

    let read_stdout = spawn_output_reader(id, tasks.clone(), stdout, false);
    let read_stderr = spawn_output_reader(id, tasks.clone(), stderr, true);

    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            await_output(read_stdout, read_stderr).await;
            info!(task_id = %id, exit_code = ?status.code(), "execution task finished");
            tasks.publish(id, Event::Finished(status.code())).await;
        }
        Ok(Err(error)) => {
            warn!(task_id = %id, error = %error, "execution task failed while waiting");
            await_output(read_stdout, read_stderr).await;
            tasks.publish(id, Event::Failed(error.to_string())).await;
        }
        Err(_) => {
            info!(task_id = %id, "execution task timed out");
            let _ = child.kill().await;
            let _ = child.wait().await;
            await_output(read_stdout, read_stderr).await;
            tasks.publish(id, Event::TimedOut).await;
        }
    }
}

fn spawn_output_reader<R>(
    id: Uuid,
    tasks: TaskStore,
    pipe: Option<R>,
    stderr: bool,
) -> tokio::task::JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let Some(mut pipe) = pipe else {
            return;
        };
        let mut buffer = [0_u8; 8192];
        loop {
            match pipe.read(&mut buffer).await {
                Ok(0) => break,
                Ok(length) => {
                    let data = String::from_utf8_lossy(&buffer[..length]).into_owned();
                    if stderr {
                        tasks.publish(id, Event::Stderr(data)).await;
                    } else {
                        tasks.publish(id, Event::Stdout(data)).await;
                    }
                }
                Err(error) => {
                    warn!(
                        task_id = %id,
                        error = %error,
                        stream = if stderr { "stderr" } else { "stdout" },
                        "failed to read task output"
                    );
                    break;
                }
            }
        }
    })
}

async fn await_output(stdout: tokio::task::JoinHandle<()>, stderr: tokio::task::JoinHandle<()>) {
    let _ = stdout.await;
    let _ = stderr.await;
}
