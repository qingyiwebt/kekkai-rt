mod runner;
mod stream;

use crate::runtime::{
    tasks::{Event, ExecRequest, TaskSnapshot, TaskStore},
    Sandbox,
};
use std::{sync::Arc, time::Duration};
use tracing::info;
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
        let creation = self.tasks.create().await;
        let id = creation.id;
        let tasks = self.tasks.clone();
        let sandbox = self.sandbox.clone();
        tokio::spawn(async move {
            let _lease = creation.lease;
            runner::run_task(tasks, sandbox, id, request, timeout, creation.cancel).await;
        });
        Ok(id)
    }

    pub async fn snapshot(&self, id: Uuid) -> Option<TaskSnapshot> {
        self.tasks.snapshot(id).await
    }
    pub async fn cancel(&self, id: Uuid) -> bool {
        self.tasks.cancel(id).await
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
