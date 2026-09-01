use super::{
    events::Event,
    model::{TaskSnapshot, TaskStatus},
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::{broadcast, oneshot, Notify, RwLock},
    task::JoinHandle,
};
use tracing::debug;
use uuid::Uuid;

struct Task {
    snapshot: TaskSnapshot,
    status: TaskStatus,
    history: Vec<Event>,
    tx: broadcast::Sender<Event>,
    expires: tokio::time::Instant,
}

struct ActiveTasks {
    count: AtomicUsize,
    idle: Notify,
}

struct CleanupSupervisor {
    stop: Notify,
    handle: std::sync::Mutex<Option<JoinHandle<()>>>,
}

pub(crate) struct TaskCreation {
    pub(crate) id: Uuid,
    pub(crate) lease: TaskLease,
    pub(crate) cancel: oneshot::Receiver<()>,
}

pub struct TaskLease {
    active: Arc<ActiveTasks>,
}

impl Drop for TaskLease {
    fn drop(&mut self) {
        if self.active.count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.active.idle.notify_waiters();
        }
    }
}

#[derive(Clone)]
pub struct TaskStore {
    inner: Arc<RwLock<HashMap<Uuid, Task>>>,
    cancellations: Arc<RwLock<HashMap<Uuid, oneshot::Sender<()>>>>,
    active: Arc<ActiveTasks>,
    cleanup: Arc<CleanupSupervisor>,
}

impl TaskStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            cancellations: Arc::new(RwLock::new(HashMap::new())),
            active: Arc::new(ActiveTasks {
                count: AtomicUsize::new(0),
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

    pub(crate) async fn create(&self) -> TaskCreation {
        let id = Uuid::new_v4();
        let (tx, _receiver) = broadcast::channel(128);
        let (cancel_tx, cancel) = oneshot::channel();
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
        self.active.count.fetch_add(1, Ordering::AcqRel);
        TaskCreation {
            id,
            lease: TaskLease {
                active: self.active.clone(),
            },
            cancel,
        }
    }

    pub async fn cancel(&self, id: Uuid) -> bool {
        self.cancellations
            .write()
            .await
            .remove(&id)
            .map(|sender| sender.send(()).is_ok())
            .unwrap_or(false)
    }

    pub(crate) async fn clear_cancellation(&self, id: Uuid) {
        self.cancellations.write().await.remove(&id);
    }

    pub async fn publish(&self, id: Uuid, event: Event) {
        if let Some(task) = self.inner.write().await.get_mut(&id) {
            debug!(task_id = %id, event = event.name(), "publishing task event");
            match &event {
                Event::Stdout(data) => task.snapshot.stdout.push_str(data),
                Event::Stderr(data) => task.snapshot.stderr.push_str(data),
                Event::Finished(code) => {
                    task.status = TaskStatus::Finished;
                    task.snapshot.status = task.status.as_str().into();
                    task.snapshot.exit_code = *code;
                    task.expires = tokio::time::Instant::now() + Duration::from_secs(300);
                }
                Event::TimedOut => {
                    task.status = TaskStatus::TimedOut;
                    task.snapshot.status = task.status.as_str().into();
                    task.expires = tokio::time::Instant::now() + Duration::from_secs(300);
                }
                Event::Failed(error) => {
                    task.status = TaskStatus::Failed;
                    task.snapshot.status = task.status.as_str().into();
                    task.snapshot.error = Some(error.clone());
                    task.expires = tokio::time::Instant::now() + Duration::from_secs(300);
                }
                Event::Started => {}
            }
            task.history.push(event.clone());
            let _ = task.tx.send(event);
        }
    }

    pub async fn snapshot(&self, id: Uuid) -> Option<TaskSnapshot> {
        self.inner
            .read()
            .await
            .get(&id)
            .map(|task| task.snapshot.clone())
    }

    pub async fn subscribe(&self, id: Uuid) -> Option<(Vec<Event>, broadcast::Receiver<Event>)> {
        self.inner
            .read()
            .await
            .get(&id)
            .map(|task| (task.history.clone(), task.tx.subscribe()))
    }

    async fn cleanup(&self) {
        let now = tokio::time::Instant::now();
        self.inner
            .write()
            .await
            .retain(|_, task| task.expires > now);
    }

    pub(crate) async fn wait_for_idle(&self) {
        loop {
            if self.active.count.load(Ordering::Acquire) == 0 {
                return;
            }
            let notified = self.active.idle.notified();
            if self.active.count.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}
