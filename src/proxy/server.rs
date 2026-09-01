use super::process::handle_connection;
use crate::config::{SandboxConfig, ToolConfig};
use anyhow::{bail, Context};
use std::{
    collections::HashMap,
    fs,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{Mutex, Notify},
    task::JoinSet,
};
use tracing::debug;

pub const SOCKET_DESTINATION: &str = "/run/kekkai-rt-tools.socket";

#[derive(Clone, Debug)]
pub struct ToolSocketMount {
    pub source: PathBuf,
    pub destination: &'static str,
}

#[derive(Clone)]
pub struct ToolSpec {
    pub path: PathBuf,
    pub env_path: Option<PathBuf>,
}

pub struct ToolProxy {
    state: Arc<ProxyState>,
    listener: Mutex<Option<tokio::task::JoinHandle<()>>>,
    socket_path: PathBuf,
}

impl ToolProxy {
    pub async fn start(
        cfg: &SandboxConfig,
        configured_tools: &HashMap<String, ToolConfig>,
    ) -> anyhow::Result<Option<Self>> {
        if configured_tools.is_empty() {
            return Ok(None);
        }

        let socket_dir = cfg.managed_bundle_dir.join("tools");
        fs::create_dir_all(&socket_dir)
            .with_context(|| format!("create tool socket directory {}", socket_dir.display()))?;
        let socket_path = socket_dir.join("kekkai-rt-tools.socket");
        remove_stale_socket(&socket_path)?;
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("bind tool socket {}", socket_path.display()))?;
        let tools = configured_tools
            .iter()
            .map(|(name, tool)| {
                (
                    name.clone(),
                    ToolSpec {
                        path: tool.path.clone(),
                        env_path: tool.env.clone(),
                    },
                )
            })
            .collect();
        let state = Arc::new(ProxyState::new(tools));
        let listener_state = state.clone();
        let listener_task = tokio::spawn(async move {
            accept_loop(listener, listener_state).await;
        });
        Ok(Some(Self {
            state,
            listener: Mutex::new(Some(listener_task)),
            socket_path,
        }))
    }

    pub fn socket_mounts(&self) -> Vec<ToolSocketMount> {
        vec![ToolSocketMount {
            source: self.socket_path.clone(),
            destination: SOCKET_DESTINATION,
        }]
    }

    #[cfg(test)]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.state.stop();
        if let Some(listener) = self.listener.lock().await.take() {
            listener.abort();
            let _ = listener.await;
        }
        self.state.shutdown_connections().await;
        remove_stale_socket(&self.socket_path)
    }
}

pub struct ProxyState {
    pub tools: Arc<HashMap<String, ToolSpec>>,
    stopping: AtomicBool,
    pub shutdown: Notify,
    connections: Mutex<JoinSet<()>>,
}
impl ProxyState {
    pub fn new(tools: HashMap<String, ToolSpec>) -> Self {
        Self {
            tools: Arc::new(tools),
            stopping: AtomicBool::new(false),
            shutdown: Notify::new(),
            connections: Mutex::new(JoinSet::new()),
        }
    }
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        self.shutdown.notify_waiters();
    }
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }
    async fn reap_finished(&self) {
        let mut tasks = self.connections.lock().await;
        while let Some(result) = tasks.try_join_next() {
            if let Err(error) = result {
                debug!(error = %error, "tool proxy connection task failed");
            }
        }
    }

    async fn add_connection(&self, stream: UnixStream, state: Arc<Self>) {
        self.reap_finished().await;
        let mut tasks = self.connections.lock().await;
        tasks.spawn(async move {
            if let Err(error) = handle_connection(stream, state).await {
                debug!(error = %error, "tool proxy connection closed");
            }
        });
    }
    pub async fn shutdown_connections(&self) {
        let mut tasks = std::mem::take(&mut *self.connections.lock().await);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !tasks.is_empty() && tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if tokio::time::timeout(remaining, tasks.join_next())
                .await
                .is_err()
            {
                break;
            }
        }
        if !tasks.is_empty() {
            debug!(
                remaining = tasks.len(),
                "aborting tool proxy connections after shutdown deadline"
            );
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        }
    }
}
pub async fn accept_loop(listener: UnixListener, state: Arc<ProxyState>) {
    loop {
        if state.is_stopping() {
            return;
        }
        let accepted = tokio::select! {
            result = listener.accept() => Some(result),
            _ = state.shutdown.notified() => None,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                state.reap_finished().await;
                continue;
            }
        };
        let Some(accepted) = accepted else {
            return;
        };
        let (stream, _) = match accepted {
            Ok(pair) => pair,
            Err(error) => {
                debug!(error = %error, "tool socket accept loop stopped");
                return;
            }
        };
        if state.is_stopping() {
            return;
        }
        state.add_connection(stream, state.clone()).await;
    }
}
pub fn remove_stale_socket(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                bail!("tool socket path is not a socket: {}", path.display());
            }
            fs::remove_file(path)
                .with_context(|| format!("remove stale tool socket {}", path.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
