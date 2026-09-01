use crate::runtime::tasks::{Event, TaskStore};
use tokio::io::{AsyncRead, AsyncReadExt};
use tracing::warn;
use uuid::Uuid;

pub(crate) fn spawn_output_reader<R>(
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
                    tasks
                        .publish(
                            id,
                            if stderr {
                                Event::Stderr(data)
                            } else {
                                Event::Stdout(data)
                            },
                        )
                        .await;
                }
                Err(error) => {
                    warn!(task_id = %id, error = %error, stream = if stderr { "stderr" } else { "stdout" }, "failed to read task output");
                    break;
                }
            }
        }
    })
}

pub(crate) async fn await_output(
    stdout: tokio::task::JoinHandle<()>,
    stderr: tokio::task::JoinHandle<()>,
) {
    let _ = stdout.await;
    let _ = stderr.await;
}
