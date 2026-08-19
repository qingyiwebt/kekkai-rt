use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{broadcast, RwLock};
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
    history: Vec<Event>,
    tx: broadcast::Sender<Event>,
    expires: tokio::time::Instant,
}
#[derive(Clone)]
pub struct TaskStore {
    inner: Arc<RwLock<HashMap<Uuid, Task>>>,
}
impl TaskStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    pub async fn create(&self) -> (Uuid, broadcast::Receiver<Event>) {
        let id = Uuid::new_v4();
        let (tx, rx) = broadcast::channel(128);
        let task = Task {
            snapshot: TaskSnapshot {
                task_id: id,
                status: "running".into(),
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                error: None,
            },
            history: Vec::new(),
            tx,
            expires: tokio::time::Instant::now() + Duration::from_secs(300),
        };
        self.inner.write().await.insert(id, task);
        (id, rx)
    }
    pub async fn publish(&self, id: Uuid, event: Event) {
        if let Some(t) = self.inner.write().await.get_mut(&id) {
            match &event {
                Event::Stdout(s) => t.snapshot.stdout.push_str(s),
                Event::Stderr(s) => t.snapshot.stderr.push_str(s),
                Event::Finished(c) => {
                    t.snapshot.status = "finished".into();
                    t.snapshot.exit_code = *c;
                    t.expires = tokio::time::Instant::now() + Duration::from_secs(300);
                }
                Event::TimedOut => {
                    t.snapshot.status = "timed_out".into();
                    t.expires = tokio::time::Instant::now() + Duration::from_secs(300);
                }
                Event::Failed(e) => {
                    t.snapshot.status = "failed".into();
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn completed_task_keeps_event_history_and_snapshot() {
        let store = TaskStore::new();
        let (id, _) = store.create().await;
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
}
