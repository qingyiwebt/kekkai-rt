use crate::config::{SandboxConfig, ToolConfig};
use anyhow::{anyhow, Context};
use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    os::unix::{ffi::OsStringExt, fs::FileTypeExt, process::ExitStatusExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};
use tokio::{
    fs::File,
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom},
    net::{UnixListener, UnixStream},
    process::Command,
    sync::{Mutex, Notify},
    task::JoinHandle,
    time::{timeout, Duration, Instant},
};
use tracing::debug;

const MAX_FIELD_BYTES: u32 = 1024 * 1024;
const MAX_ARGUMENTS: u32 = 1024;
const MAX_ENVIRONMENT: u32 = 1024;
const TOOL_START_FAILURE: i32 = 127;

pub(crate) const SOCKET_DESTINATIONS: [&str; 4] = [
    "/run/agentcell-tools.socket",
    "/run/agentcell-tools-stdout.socket",
    "/run/agentcell-tools-stderr.socket",
    "/run/agentcell-tools-status.socket",
];

#[derive(Clone, Copy)]
enum SocketKind {
    Submit,
    Stdout,
    Stderr,
    Status,
}

#[derive(Clone, Debug)]
pub(crate) struct ToolSocketMount {
    pub(crate) source: PathBuf,
    pub(crate) destination: &'static str,
}

#[derive(Clone)]
struct ToolSpec {
    path: PathBuf,
    env_path: PathBuf,
}

struct RequestRecord {
    stdout: Arc<StreamBuffer>,
    stderr: Arc<StreamBuffer>,
    status: StatusBuffer,
}

struct StreamBuffer {
    path: PathBuf,
    file: Mutex<File>,
    complete: AtomicBool,
    reader_active: AtomicBool,
    consumed: AtomicBool,
    notify: Notify,
}

struct StatusBuffer {
    code: Mutex<Option<i32>>,
    consumed: AtomicBool,
    notify: Notify,
}

struct ProxyState {
    tools: Arc<HashMap<String, ToolSpec>>,
    results_dir: PathBuf,
    next_result_id: AtomicU64,
    registration: Notify,
    requests: Mutex<HashMap<String, Arc<RequestRecord>>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

pub(crate) struct ToolProxy {
    state: Arc<ProxyState>,
    listeners: Mutex<Vec<JoinHandle<()>>>,
    socket_paths: Vec<PathBuf>,
}

impl StreamBuffer {
    fn new(path: PathBuf) -> anyhow::Result<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("create tool output file {}", path.display()))?;
        Ok(Self {
            path,
            file: Mutex::new(File::from_std(file)),
            complete: AtomicBool::new(false),
            reader_active: AtomicBool::new(false),
            consumed: AtomicBool::new(false),
            notify: Notify::new(),
        })
    }

    async fn append(&self, bytes: &[u8]) -> anyhow::Result<()> {
        let mut file = self.file.lock().await;
        file.write_all(bytes).await?;
        file.flush().await?;
        self.notify.notify_one();
        Ok(())
    }

    fn finish(&self) {
        self.complete.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    async fn read_chunk(&self, offset: u64) -> anyhow::Result<(Vec<u8>, bool)> {
        let length = tokio::fs::metadata(&self.path).await?.len();
        if offset >= length {
            return Ok((Vec::new(), self.complete.load(Ordering::Acquire)));
        }
        let amount = (length - offset).min(8192) as usize;
        let mut file = File::open(&self.path).await?;
        file.seek(SeekFrom::Start(offset)).await?;
        let mut bytes = vec![0_u8; amount];
        file.read_exact(&mut bytes).await?;
        Ok((bytes, false))
    }
}

impl StatusBuffer {
    fn new() -> Self {
        Self {
            code: Mutex::new(None),
            consumed: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    async fn set(&self, code: i32) {
        *self.code.lock().await = Some(code);
        self.notify.notify_waiters();
    }
}

impl RequestRecord {
    fn new(results_dir: &Path, result_id: u64) -> anyhow::Result<Self> {
        Ok(Self {
            stdout: Arc::new(StreamBuffer::new(
                results_dir.join(format!("{result_id}.stdout")),
            )?),
            stderr: Arc::new(StreamBuffer::new(
                results_dir.join(format!("{result_id}.stderr")),
            )?),
            status: StatusBuffer::new(),
        })
    }
}

impl ToolProxy {
    pub(crate) async fn start(
        cfg: &SandboxConfig,
        configured_tools: &HashMap<String, ToolConfig>,
    ) -> anyhow::Result<Option<Self>> {
        if configured_tools.is_empty() {
            return Ok(None);
        }

        let socket_dir = cfg.managed_bundle_dir.join("tools");
        fs::create_dir_all(&socket_dir)
            .with_context(|| format!("create tool socket directory {}", socket_dir.display()))?;
        let results_dir = socket_dir.join("results");
        fs::create_dir_all(&results_dir)
            .with_context(|| format!("create tool results directory {}", results_dir.display()))?;

        let mut socket_paths: Vec<PathBuf> = Vec::new();
        let mut listeners = Vec::new();
        for name in [
            "agentcell-tools.socket",
            "agentcell-tools-stdout.socket",
            "agentcell-tools-stderr.socket",
            "agentcell-tools-status.socket",
        ] {
            let path = socket_dir.join(name);
            remove_stale_socket(&path)?;
            let listener = match UnixListener::bind(&path) {
                Ok(listener) => listener,
                Err(error) => {
                    for bound_path in &socket_paths {
                        let _ = remove_stale_socket(bound_path);
                    }
                    return Err(error)
                        .with_context(|| format!("bind tool socket {}", path.display()));
                }
            };
            socket_paths.push(path);
            listeners.push(listener);
        }

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
        let state = Arc::new(ProxyState {
            tools: Arc::new(tools),
            results_dir,
            next_result_id: AtomicU64::new(1),
            registration: Notify::new(),
            requests: Mutex::new(HashMap::new()),
            tasks: Mutex::new(Vec::new()),
        });

        let kinds = [
            SocketKind::Submit,
            SocketKind::Stdout,
            SocketKind::Stderr,
            SocketKind::Status,
        ];
        let mut listener_tasks = Vec::new();
        for (listener, kind) in listeners.into_iter().zip(kinds) {
            let listener_state = state.clone();
            listener_tasks.push(tokio::spawn(async move {
                accept_loop(listener, kind, listener_state).await;
            }));
        }

        Ok(Some(Self {
            state,
            listeners: Mutex::new(listener_tasks),
            socket_paths,
        }))
    }

    pub(crate) fn socket_mounts(&self) -> Vec<ToolSocketMount> {
        self.socket_paths
            .iter()
            .cloned()
            .zip(SOCKET_DESTINATIONS)
            .map(|(source, destination)| ToolSocketMount {
                source,
                destination,
            })
            .collect()
    }

    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        for task in self.listeners.lock().await.drain(..) {
            task.abort();
        }
        for task in self.state.tasks.lock().await.drain(..) {
            task.abort();
        }
        for path in &self.socket_paths {
            remove_stale_socket(path)?;
        }
        if self.state.results_dir.exists() {
            fs::remove_dir_all(&self.state.results_dir).with_context(|| {
                format!(
                    "remove tool results directory {}",
                    self.state.results_dir.display()
                )
            })?;
        }
        Ok(())
    }
}

async fn accept_loop(listener: UnixListener, kind: SocketKind, state: Arc<ProxyState>) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(error) => {
                debug!(kind = kind_name(kind), error = %error, "tool socket accept loop stopped");
                return;
            }
        };
        let connection_state = state.clone();
        let task = tokio::spawn(async move {
            let result = match kind {
                SocketKind::Submit => handle_submit(stream, connection_state.clone()).await,
                SocketKind::Stdout => handle_stream(stream, connection_state.clone(), false).await,
                SocketKind::Stderr => handle_stream(stream, connection_state.clone(), true).await,
                SocketKind::Status => handle_status(stream, connection_state.clone()).await,
            };
            if let Err(error) = result {
                debug!(kind = kind_name(kind), error = %error, "tool socket connection closed");
            }
        });
        state.tasks.lock().await.push(task);
    }
}

fn kind_name(kind: SocketKind) -> &'static str {
    match kind {
        SocketKind::Submit => "submit",
        SocketKind::Stdout => "stdout",
        SocketKind::Stderr => "stderr",
        SocketKind::Status => "status",
    }
}

async fn handle_submit(mut stream: UnixStream, state: Arc<ProxyState>) -> anyhow::Result<()> {
    let request = read_request_header(&mut stream).await?;
    let result_id = state.next_result_id.fetch_add(1, Ordering::Relaxed);
    let record = Arc::new(RequestRecord::new(&state.results_dir, result_id)?);
    {
        let mut requests = state.requests.lock().await;
        if requests.contains_key(&request.id) {
            return Err(anyhow!("duplicate request id"));
        }
        requests.insert(request.id.clone(), record.clone());
    }
    state.registration.notify_waiters();

    let Some(tool) = state.tools.get(&request.command).cloned() else {
        fail_request(
            &record,
            format!("unknown tool command: {}\n", request.command),
        )
        .await;
        return Ok(());
    };

    let environment = match load_dotenv(&tool.env_path) {
        Ok(environment) => environment,
        Err(error) => {
            fail_request(
                &record,
                format!("failed to load tool environment: {error}\n"),
            )
            .await;
            return Ok(());
        }
    };

    let mut command = Command::new(&tool.path);
    command
        .env_clear()
        .args(request.args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in environment {
        command.env(key, value);
    }
    for (key, value) in request.environment {
        command.env(key, value);
    }
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            fail_request(
                &record,
                format!("failed to start tool {}: {error}\n", request.command),
            )
            .await;
            return Ok(());
        }
    };

    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_task = tokio::spawn(drain_output(stdout, record.stdout.clone()));
    let stderr_task = tokio::spawn(drain_output(stderr, record.stderr.clone()));
    let request_id = request.id.clone();
    let job = tokio::spawn(async move {
        let code = if let Some(mut stdin) = stdin {
            let mut input = Box::pin(tokio::io::copy(&mut stream, &mut stdin));
            tokio::select! {
                status = child.wait() => match status {
                    Ok(status) => exit_code(status),
                    Err(error) => {
                        let _ = record
                            .stderr
                            .append(format!("failed to wait for tool: {error}\n").as_bytes())
                            .await;
                        TOOL_START_FAILURE
                    }
                },
                input_result = &mut input => {
                    if let Err(error) = input_result {
                        let _ = record
                            .stderr
                            .append(format!("stdin socket disconnected: {error}\n").as_bytes())
                            .await;
                    }
                    let _ = record
                        .stderr
                        .append(b"stdin socket closed; terminating tool\n")
                        .await;
                    kill_process_group(&mut child).await;
                    match child.wait().await {
                        Ok(status) => exit_code(status),
                        Err(error) => {
                            let _ = record
                                .stderr
                                .append(format!("failed to wait for terminated tool: {error}\n").as_bytes())
                                .await;
                            TOOL_START_FAILURE
                        }
                    }
                }
            }
        } else {
            match child.wait().await {
                Ok(status) => exit_code(status),
                Err(error) => {
                    let _ = record
                        .stderr
                        .append(format!("failed to wait for tool: {error}\n").as_bytes())
                        .await;
                    TOOL_START_FAILURE
                }
            }
        };
        let _ = stdout_task.await;
        let _ = stderr_task.await;
        record.stdout.finish();
        record.stderr.finish();
        record.status.set(code).await;
        debug!(request_id = %request_id, exit_code = code, "tool request finished");
    });
    state.tasks.lock().await.push(job);
    Ok(())
}

fn load_dotenv(path: &Path) -> anyhow::Result<HashMap<String, String>> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("read tool environment {}", path.display()))?;
    let mut values = HashMap::new();
    for (line_number, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (raw_key, raw_value) = line
            .split_once('=')
            .ok_or_else(|| anyhow!("line {} must use KEY=VALUE", line_number + 1))?;
        let key = raw_key.trim();
        if key.is_empty() || key.contains('=') || key.contains('\0') {
            return Err(anyhow!(
                "line {} has an invalid environment key",
                line_number + 1
            ));
        }
        let mut value = raw_value.trim().to_owned();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = value[1..value.len() - 1].to_owned();
        }
        if value.contains('\0') {
            return Err(anyhow!("line {} contains NUL", line_number + 1));
        }
        values.insert(key.to_owned(), value);
    }
    Ok(values)
}

async fn fail_request(record: &RequestRecord, message: String) {
    let _ = record.stderr.append(message.as_bytes()).await;
    record.stdout.finish();
    record.stderr.finish();
    record.status.set(TOOL_START_FAILURE).await;
}

async fn drain_output<R>(pipe: Option<R>, buffer: Arc<StreamBuffer>) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
{
    let Some(mut pipe) = pipe else {
        return Ok(());
    };
    let mut bytes = [0_u8; 8192];
    loop {
        let length = pipe.read(&mut bytes).await?;
        if length == 0 {
            return Ok(());
        }
        buffer.append(&bytes[..length]).await?;
    }
}

async fn handle_stream(
    mut stream: UnixStream,
    state: Arc<ProxyState>,
    stderr: bool,
) -> anyhow::Result<()> {
    let id = read_id(&mut stream).await?;
    let record = wait_for_request(&state, &id).await;
    let Some(record) = record else {
        stream.write_all(b"ERR unknown request\n").await?;
        return Ok(());
    };
    let buffer = if stderr {
        &record.stderr
    } else {
        &record.stdout
    };
    if buffer
        .reader_active
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        stream
            .write_all(b"ERR stream already has a reader\n")
            .await?;
        return Ok(());
    }

    let mut offset = 0_u64;
    let result = async {
        loop {
            let notified = buffer.notify.notified();
            let (chunk, done) = buffer.read_chunk(offset).await?;
            if !chunk.is_empty() {
                stream.write_all(&chunk).await?;
                offset += chunk.len() as u64;
            } else if done {
                break;
            } else {
                notified.await;
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    buffer.reader_active.store(false, Ordering::Release);
    if result.is_ok() {
        buffer.consumed.store(true, Ordering::Release);
        maybe_cleanup(&state, &id).await;
    }
    result
}

async fn handle_status(mut stream: UnixStream, state: Arc<ProxyState>) -> anyhow::Result<()> {
    let id = read_id(&mut stream).await?;
    let Some(record) = wait_for_request(&state, &id).await else {
        stream.write_all(b"ERR unknown request\n").await?;
        return Ok(());
    };
    let code = loop {
        let notified = record.status.notify.notified();
        if let Some(code) = *record.status.code.lock().await {
            break code;
        }
        notified.await;
    };
    stream.write_all(format!("{code}\n").as_bytes()).await?;
    record.status.consumed.store(true, Ordering::Release);
    maybe_cleanup(&state, &id).await;
    Ok(())
}

async fn lookup_request(state: &ProxyState, id: &str) -> Option<Arc<RequestRecord>> {
    state.requests.lock().await.get(id).cloned()
}

async fn wait_for_request(state: &ProxyState, id: &str) -> Option<Arc<RequestRecord>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(record) = lookup_request(state, id).await {
            return Some(record);
        }
        let notified = state.registration.notified();
        if let Some(record) = lookup_request(state, id).await {
            return Some(record);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || timeout(remaining, notified).await.is_err() {
            return None;
        }
    }
}

async fn maybe_cleanup(state: &ProxyState, id: &str) {
    let Some(record) = lookup_request(state, id).await else {
        return;
    };
    if record.stdout.consumed.load(Ordering::Acquire)
        && record.stderr.consumed.load(Ordering::Acquire)
        && record.status.consumed.load(Ordering::Acquire)
    {
        let _ = fs::remove_file(&record.stdout.path);
        let _ = fs::remove_file(&record.stderr.path);
        state.requests.lock().await.remove(id);
    }
}

struct RequestHeader {
    id: String,
    command: String,
    args: Vec<OsString>,
    environment: Vec<(OsString, OsString)>,
}

async fn read_request_header(stream: &mut UnixStream) -> anyhow::Result<RequestHeader> {
    let id = read_id(stream).await?;
    let argc = read_u32(stream).await?;
    if argc > MAX_ARGUMENTS {
        return Err(anyhow!("argument count exceeds {MAX_ARGUMENTS}"));
    }
    let envc = read_u32(stream).await?;
    if envc > MAX_ENVIRONMENT {
        return Err(anyhow!("environment count exceeds {MAX_ENVIRONMENT}"));
    }
    let command =
        String::from_utf8(read_field(stream).await?).context("tool command is not valid UTF-8")?;
    if command.is_empty() {
        return Err(anyhow!("tool command must not be empty"));
    }

    let mut args = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        args.push(os_string(read_field(stream).await?));
    }
    let mut environment = Vec::with_capacity(envc as usize);
    for _ in 0..envc {
        let key = String::from_utf8(read_field(stream).await?)
            .context("environment key is not valid UTF-8")?;
        if key.is_empty() || key.contains('=') {
            return Err(anyhow!("invalid environment key"));
        }
        environment.push((OsString::from(key), os_string(read_field(stream).await?)));
    }
    Ok(RequestHeader {
        id,
        command,
        args,
        environment,
    })
}

async fn read_id(stream: &mut UnixStream) -> anyhow::Result<String> {
    let id = String::from_utf8(read_field(stream).await?).context("request id is not UTF-8")?;
    if id.is_empty() {
        return Err(anyhow!("request id must not be empty"));
    }
    Ok(id)
}

async fn read_field(stream: &mut UnixStream) -> anyhow::Result<Vec<u8>> {
    let length = read_u32(stream).await?;
    if length > MAX_FIELD_BYTES {
        return Err(anyhow!("field length exceeds {MAX_FIELD_BYTES}"));
    }
    let mut value = vec![0_u8; length as usize];
    stream.read_exact(&mut value).await?;
    Ok(value)
}

async fn read_u32(stream: &mut UnixStream) -> anyhow::Result<u32> {
    let mut bytes = [0_u8; 4];
    stream.read_exact(&mut bytes).await?;
    Ok(u32::from_be_bytes(bytes))
}

fn os_string(bytes: Vec<u8>) -> OsString {
    OsString::from_vec(bytes)
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(TOOL_START_FAILURE)
}

async fn kill_process_group(child: &mut tokio::process::Child) {
    let Some(pid) = child.id() else {
        return;
    };
    let killed = unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) } == 0;
    if !killed {
        let _ = child.kill().await;
    }
}

fn remove_stale_socket(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                return Err(anyhow!(
                    "tool socket path is not a socket: {}",
                    path.display()
                ));
            }
            fs::remove_file(path)
                .with_context(|| format!("remove stale tool socket {}", path.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::fs::PermissionsExt, time::Duration};
    use tempfile::tempdir;
    use tokio::io::AsyncWriteExt;

    fn config(root: &Path) -> SandboxConfig {
        let mut config: SandboxConfig = toml::from_str("rootfs_dir = \".\"").unwrap();
        config.rootfs_dir = root.to_path_buf();
        config.managed_bundle_dir = root.join("bundle");
        config
    }

    #[tokio::test]
    async fn proxy_executes_configured_tool_and_routes_streams_and_status() {
        let temp = tempdir().unwrap();
        let script = temp.path().join("tool.sh");
        fs::write(
            &script,
            "#!/bin/sh\nprintf 'out:hello'\nprintf 'err:%s' \"$TOOL_SECRET\" >&2\nexit 7\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let env = temp.path().join("tool.env");
        fs::write(&env, "TOOL_SECRET=secret\n").unwrap();

        let cfg = config(temp.path());
        let mut configured_tools = HashMap::new();
        configured_tools.insert("something-cli".into(), ToolConfig { path: script, env });
        let proxy = ToolProxy::start(&cfg, &configured_tools)
            .await
            .unwrap()
            .unwrap();
        let paths = proxy.socket_paths.clone();

        let mut submit = UnixStream::connect(&paths[0]).await.unwrap();
        write_field(&mut submit, b"request-1").await;
        submit.write_all(&0_u32.to_be_bytes()).await.unwrap();
        submit.write_all(&0_u32.to_be_bytes()).await.unwrap();
        write_field(&mut submit, b"something-cli").await;
        submit.write_all(b"hello\n").await.unwrap();

        let mut stdout = UnixStream::connect(&paths[1]).await.unwrap();
        write_field(&mut stdout, b"request-1").await;
        let mut stdout_data = Vec::new();
        stdout.read_to_end(&mut stdout_data).await.unwrap();

        let mut stderr = UnixStream::connect(&paths[2]).await.unwrap();
        write_field(&mut stderr, b"request-1").await;
        let mut stderr_data = Vec::new();
        stderr.read_to_end(&mut stderr_data).await.unwrap();

        let mut status = UnixStream::connect(&paths[3]).await.unwrap();
        write_field(&mut status, b"request-1").await;
        let mut status_data = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), status.read_to_end(&mut status_data))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(stdout_data, b"out:hello");
        assert_eq!(stderr_data, b"err:secret");
        assert_eq!(status_data, b"7\n");
        proxy.shutdown().await.unwrap();
    }

    #[test]
    fn dotenv_is_loaded_again_after_the_file_changes() {
        let temp = tempdir().unwrap();
        let env = temp.path().join("tool.env");
        fs::write(&env, "SECRET=one\n").unwrap();
        assert_eq!(
            load_dotenv(&env).unwrap().get("SECRET"),
            Some(&"one".into())
        );

        fs::write(&env, "SECRET=two\n").unwrap();
        assert_eq!(
            load_dotenv(&env).unwrap().get("SECRET"),
            Some(&"two".into())
        );
    }

    #[tokio::test]
    async fn disconnecting_submit_socket_terminates_the_tool() {
        let temp = tempdir().unwrap();
        let script = temp.path().join("long-tool.sh");
        fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let env = temp.path().join("tool.env");
        fs::write(&env, "").unwrap();

        let cfg = config(temp.path());
        let configured_tools =
            HashMap::from([("long-tool".into(), ToolConfig { path: script, env })]);
        let proxy = ToolProxy::start(&cfg, &configured_tools)
            .await
            .unwrap()
            .unwrap();
        let paths = proxy.socket_paths.clone();

        let mut submit = UnixStream::connect(&paths[0]).await.unwrap();
        write_field(&mut submit, b"disconnect-1").await;
        submit.write_all(&0_u32.to_be_bytes()).await.unwrap();
        submit.write_all(&0_u32.to_be_bytes()).await.unwrap();
        write_field(&mut submit, b"long-tool").await;
        drop(submit);

        let mut status = UnixStream::connect(&paths[3]).await.unwrap();
        write_field(&mut status, b"disconnect-1").await;
        let mut status_data = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), status.read_to_end(&mut status_data))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(status_data, b"137\n");
        proxy.shutdown().await.unwrap();
    }

    async fn write_field(stream: &mut UnixStream, value: &[u8]) {
        stream
            .write_all(&(value.len() as u32).to_be_bytes())
            .await
            .unwrap();
        stream.write_all(value).await.unwrap();
    }
}
