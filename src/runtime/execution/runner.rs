use super::stream::{await_output, spawn_output_reader};
use crate::runtime::{
    tasks::{Event, ExecRequest, TaskStore},
    RunningExec, Sandbox,
};
use std::{sync::Arc, time::Duration};
use tokio::{io::AsyncWriteExt, sync::oneshot};
use tracing::{debug, info, warn};
use uuid::Uuid;

pub(crate) async fn run_task(
    tasks: TaskStore,
    sandbox: Arc<Sandbox>,
    id: Uuid,
    request: ExecRequest,
    timeout: Duration,
    cancel: oneshot::Receiver<()>,
) {
    info!(task_id = %id, program = %request.argv.first().map(String::as_str).unwrap_or(""), argc = request.argv.len(), timeout_seconds = timeout.as_secs(), "execution task started");
    tasks.publish(id, Event::Started).await;
    let running = match sandbox.exec(&request).await {
        Ok(running) => running,
        Err(error) => {
            warn!(task_id = %id, error = %error, "failed to start execution task");
            tasks.publish(id, Event::Failed(error.to_string())).await;
            tasks.clear_cancellation(id).await;
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
    let wait = async {
        tokio::select! {
            result = child.wait() => result.map(WaitOutcome::Finished),
            _ = cancel => { info!(task_id = %id, "execution task cancelled"); let _ = child.kill().await; let _ = child.wait().await; Ok(WaitOutcome::Cancelled) }
        }
    };
    match tokio::time::timeout(timeout, wait).await {
        Ok(Ok(WaitOutcome::Finished(status))) => {
            await_output(read_stdout, read_stderr).await;
            info!(task_id = %id, exit_code = ?status.code(), "execution task finished");
            tasks.publish(id, Event::Finished(status.code())).await;
        }
        Ok(Ok(WaitOutcome::Cancelled)) => {
            await_output(read_stdout, read_stderr).await;
            tasks.publish(id, Event::Finished(None)).await;
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
    tasks.clear_cancellation(id).await;
}

enum WaitOutcome {
    Finished(std::process::ExitStatus),
    Cancelled,
}
