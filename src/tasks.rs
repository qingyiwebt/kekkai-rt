use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    sync::{broadcast, oneshot, Notify, RwLock},
    task::JoinHandle,
};
use tracing::debug;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize)]
pub struct ExecRequest {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub stdin: Option<String>,
    pub timeout_seconds: Option<u64>,
}
#[derive(Clone, Debug, Serialize)]
pub struct TaskSnapshot {
    pub task_id: Uuid,
    pub status: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaskStatus {
    Running,
    Finished,
    TimedOut,
    Failed,
}

impl TaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Finished => "finished",
            Self::TimedOut => "timed_out",
            Self::Failed => "failed",
        }
    }
}
#[derive(Clone, Debug)]
pub enum Event {
    Started,
    Stdout(String),
    Stderr(String),
    Finished(Option<i32>),
    TimedOut,
    Failed(String),
}
impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Stdout(_) => "stdout",
            Self::Stderr(_) => "stderr",
            Self::Finished(_) => "finished",
            Self::TimedOut => "timed_out",
            Self::Failed(_) => "failed",
        }
    }
    pub fn data(&self) -> String {
        serde_json::to_string(&match self {
            Self::Started => serde_json::json!({}),
            Self::Stdout(s) => serde_json::json!({"data":s}),
            Self::Stderr(s) => serde_json::json!({"data":s}),
            Self::Finished(c) => serde_json::json!({"exit_code":c}),
            Self::TimedOut => serde_json::json!({}),
            Self::Failed(e) => serde_json::json!({"error":e}),
        })
        .unwrap()
    }
}
struct Task {
    snapshot: TaskSnapshot,
    status: TaskStatus,
    history: Vec<Event>,
    tx: broadcast::Sender<Event>,
    expires: tokio::time::Instant,
}
#[derive(Clone)]
pub struct TaskStore {
    inner: Arc<RwLock<HashMap<Uuid, Task>>>,
    cancellations: Arc<RwLock<HashMap<Uuid, oneshot::Sender<()>>>>,
    active: Arc<ActiveTasks>,
    cleanup: Arc<CleanupSupervisor>,
}

struct ActiveTasks {
    count: std::sync::atomic::AtomicUsize,
    idle: Notify,
}

struct CleanupSupervisor {
    stop: Notify,
    handle: std::sync::Mutex<Option<JoinHandle<()>>>,
}

pub struct TaskLease {
    active: Arc<ActiveTasks>,
}

impl Drop for TaskLease {
    fn drop(&mut self) {
        if self
            .active
            .count
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
            == 1
        {
            self.active.idle.notify_waiters();
        }
    }
}
impl TaskStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            cancellations: Arc::new(RwLock::new(HashMap::new())),
            active: Arc::new(ActiveTasks {
                count: std::sync::atomic::AtomicUsize::new(0),
                idle: Notify::new(),
            }),
            cleanup: Arc::new(CleanupSupervisor {
                stop: Notify::new(),
                handle: std::sync::Mutex::new(None),
            }),
        }
    }

    pub fn start_cleanup(&self) {
        let Ok(mut handle) = self.cleanup.handle.lock() else {
            return;
        };
        if handle.is_some() {
            return;
        }
        let tasks = self.clone();
        let stop = self.cleanup.clone();
        *handle = Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                tokio::select! {
                    _ = interval.tick() => tasks.cleanup().await,
                    _ = stop.stop.notified() => break,
                }
            }
        }));
    }

    pub async fn shutdown_cleanup(&self) {
        self.cleanup.stop.notify_one();
        let handle = self
            .cleanup
            .handle
            .lock()
            .ok()
            .and_then(|mut handle| handle.take());
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }
    pub async fn create(
        &self,
    ) -> (
        Uuid,
        broadcast::Receiver<Event>,
        TaskLease,
        oneshot::Receiver<()>,
    ) {
        let id = Uuid::new_v4();
        let (tx, rx) = broadcast::channel(128);
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let task = Task {
            snapshot: TaskSnapshot {
                task_id: id,
                status: TaskStatus::Running.as_str().into(),
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                error: None,
            },
            status: TaskStatus::Running,
            history: Vec::new(),
            tx,
            expires: tokio::time::Instant::now() + Duration::from_secs(300),
        };
        self.inner.write().await.insert(id, task);
        self.cancellations.write().await.insert(id, cancel_tx);
        self.active
            .count
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        (
            id,
            rx,
            TaskLease {
                active: self.active.clone(),
            },
            cancel_rx,
        )
    }
    pub async fn cancel(&self, id: Uuid) -> bool {
        self.cancellations
            .write()
            .await
            .remove(&id)
            .map(|sender| sender.send(()).is_ok())
            .unwrap_or(false)
    }
    pub async fn clear_cancellation(&self, id: Uuid) {
        self.cancellations.write().await.remove(&id);
    }
    pub async fn publish(&self, id: Uuid, event: Event) {
        if let Some(t) = self.inner.write().await.get_mut(&id) {
            debug!(task_id = %id, event = event.name(), "publishing task event");
            match &event {
                Event::Stdout(s) => t.snapshot.stdout.push_str(s),
                Event::Stderr(s) => t.snapshot.stderr.push_str(s),
                Event::Finished(c) => {
                    t.status = TaskStatus::Finished;
                    t.snapshot.status = t.status.as_str().into();
                    t.snapshot.exit_code = *c;
                    t.expires = tokio::time::Instant::now() + Duration::from_secs(300);
                }
                Event::TimedOut => {
                    t.status = TaskStatus::TimedOut;
                    t.snapshot.status = t.status.as_str().into();
                    t.expires = tokio::time::Instant::now() + Duration::from_secs(300);
                }
                Event::Failed(e) => {
                    t.status = TaskStatus::Failed;
                    t.snapshot.status = t.status.as_str().into();
                    t.snapshot.error = Some(e.clone());
                    t.expires = tokio::time::Instant::now() + Duration::from_secs(300);
                }
                Event::Started => {}
            }
            t.history.push(event.clone());
            let _ = t.tx.send(event);
        }
    }
    pub async fn snapshot(&self, id: Uuid) -> Option<TaskSnapshot> {
        self.inner.read().await.get(&id).map(|t| t.snapshot.clone())
    }
    pub async fn subscribe(&self, id: Uuid) -> Option<(Vec<Event>, broadcast::Receiver<Event>)> {
        self.inner
            .read()
            .await
            .get(&id)
            .map(|t| (t.history.clone(), t.tx.subscribe()))
    }
    pub async fn cleanup(&self) {
        let now = tokio::time::Instant::now();
        self.inner.write().await.retain(|_, t| t.expires > now);
    }

    pub async fn wait_for_idle(&self) {
        loop {
            if self.active.count.load(std::sync::atomic::Ordering::Acquire) == 0 {
                return;
            }
            let notified = self.active.idle.notified();
            if self.active.count.load(std::sync::atomic::Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn completed_task_keeps_event_history_and_snapshot() {
        let store = TaskStore::new();
        let (id, _, _lease, _cancel) = store.create().await;
        store.publish(id, Event::Started).await;
        store.publish(id, Event::Stdout("hello".into())).await;
        store.publish(id, Event::Finished(Some(0))).await;

        let snapshot = store.snapshot(id).await.unwrap();
        assert_eq!(snapshot.status, "finished");
        assert_eq!(snapshot.exit_code, Some(0));
        assert_eq!(snapshot.stdout, "hello");

        let (history, _) = store.subscribe(id).await.unwrap();
        assert_eq!(history.len(), 3);
        assert!(matches!(history[2], Event::Finished(Some(0))));
    }

    #[tokio::test]
    async fn cancellation_notifies_the_execution_task() {
        let store = TaskStore::new();
        let (id, _, _lease, cancel) = store.create().await;
        assert!(store.cancel(id).await);
        assert!(cancel.await.is_ok());
        assert!(!store.cancel(id).await);
    }

    #[tokio::test]
    async fn shutdown_waits_until_task_lease_is_released() {
        let store = TaskStore::new();
        let (_, _, lease, _cancel) = store.create().await;
        let waiting = tokio::spawn({
            let store = store.clone();
            async move { store.wait_for_idle().await }
        });

        assert!(!waiting.is_finished());
        drop(lease);
        waiting.await.unwrap();
    }

    #[tokio::test]
    async fn cleanup_supervisor_can_stop_without_waiting_for_interval() {
        let store = TaskStore::new();
        store.start_cleanup();
        store.shutdown_cleanup().await;
        store.shutdown_cleanup().await;
    }
}
