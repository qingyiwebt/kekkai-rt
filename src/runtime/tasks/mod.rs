mod events;
mod model;
mod store;

pub use events::Event;
pub use model::{ExecRequest, TaskSnapshot};
pub use store::TaskStore;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn completed_task_keeps_event_history_and_snapshot() {
        let store = TaskStore::new();
        let creation = store.create().await;
        store.publish(creation.id, Event::Started).await;
        store
            .publish(creation.id, Event::Stdout("hello".into()))
            .await;
        store.publish(creation.id, Event::Finished(Some(0))).await;
        let snapshot = store.snapshot(creation.id).await.unwrap();
        assert_eq!(snapshot.status, "finished");
        assert_eq!(snapshot.exit_code, Some(0));
        assert_eq!(snapshot.stdout, "hello");
        let (history, _) = store.subscribe(creation.id).await.unwrap();
        assert_eq!(history.len(), 3);
        assert!(matches!(history[2], Event::Finished(Some(0))));
    }

    #[tokio::test]
    async fn cancellation_notifies_the_execution_task() {
        let store = TaskStore::new();
        let creation = store.create().await;
        assert!(store.cancel(creation.id).await);
        assert!(creation.cancel.await.is_ok());
        assert!(!store.cancel(creation.id).await);
    }

    #[tokio::test]
    async fn shutdown_waits_until_task_lease_is_released() {
        let store = TaskStore::new();
        let creation = store.create().await;
        let waiting = tokio::spawn({
            let store = store.clone();
            async move { store.wait_for_idle().await }
        });
        assert!(!waiting.is_finished());
        drop(creation.lease);
        waiting.await.unwrap();
    }

    #[tokio::test]
    async fn cleanup_supervisor_can_stop_without_waiting_for_interval() {
        let store = TaskStore::new();
        store.start_cleanup();
        store.shutdown_cleanup().await;
        store.shutdown_cleanup().await;
    }

    #[test]
    fn event_wire_names_and_payloads_remain_stable() {
        assert_eq!(Event::Started.name(), "started");
        assert_eq!(Event::Stdout("hello".into()).data(), r#"{"data":"hello"}"#);
        assert_eq!(Event::Finished(Some(7)).data(), r#"{"exit_code":7}"#);
        assert_eq!(Event::TimedOut.data(), "{}");
        assert_eq!(Event::Failed("boom".into()).data(), r#"{"error":"boom"}"#);
    }
}
