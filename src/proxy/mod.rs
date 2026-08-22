use crate::config::{SandboxConfig, ToolConfig};
use anyhow::{anyhow, bail, Context};
use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    os::unix::{ffi::OsStringExt, fs::FileTypeExt, process::ExitStatusExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    process::{Child, Command},
    sync::Mutex,
    task::JoinHandle,
};
use tracing::debug;

const MAX_FRAME_BYTES: u32 = 1024 * 1024;
const MAX_FIELD_BYTES: u32 = 1024 * 1024;
const MAX_ARGUMENTS: u32 = 1024;
const TOOL_START_FAILURE: i32 = 127;

const OPEN: u8 = 1;
const STDIN: u8 = 2;
const STDIN_EOF: u8 = 3;
const STDOUT: u8 = 0x10;
const STDERR: u8 = 0x11;
const EXIT: u8 = 0x12;
const ERROR: u8 = 0x13;

pub(crate) const SOCKET_DESTINATION: &str = "/run/kekkai-rt-tools.socket";

#[derive(Clone, Debug)]
pub(crate) struct ToolSocketMount {
    pub(crate) source: PathBuf,
    pub(crate) destination: &'static str,
}

#[derive(Clone)]
struct ToolSpec {
    path: PathBuf,
    env_path: Option<PathBuf>,
}

struct ProxyState {
    tools: Arc<HashMap<String, ToolSpec>>,
    stopping: AtomicBool,
    shutdown: tokio::sync::Notify,
    connections: Mutex<Vec<JoinHandle<()>>>,
}

pub(crate) struct ToolProxy {
    state: Arc<ProxyState>,
    listener: Mutex<Option<JoinHandle<()>>>,
    socket_path: PathBuf,
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
        let state = Arc::new(ProxyState {
            tools: Arc::new(tools),
            stopping: AtomicBool::new(false),
            shutdown: tokio::sync::Notify::new(),
            connections: Mutex::new(Vec::new()),
        });
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

    pub(crate) fn socket_mounts(&self) -> Vec<ToolSocketMount> {
        vec![ToolSocketMount {
            source: self.socket_path.clone(),
            destination: SOCKET_DESTINATION,
        }]
    }

    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        self.state.stopping.store(true, Ordering::Release);
        self.state.shutdown.notify_waiters();
        if let Some(listener) = self.listener.lock().await.take() {
            listener.abort();
        }

        let connections = self
            .state
            .connections
            .lock()
            .await
            .drain(..)
            .collect::<Vec<_>>();
        for mut connection in connections {
            if tokio::time::timeout(Duration::from_secs(2), &mut connection)
                .await
                .is_err()
            {
                connection.abort();
                let _ = connection.await;
            }
        }
        remove_stale_socket(&self.socket_path)?;
        Ok(())
    }
}

async fn accept_loop(listener: UnixListener, state: Arc<ProxyState>) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(error) => {
                debug!(error = %error, "tool socket accept loop stopped");
                return;
            }
        };
        if state.stopping.load(Ordering::Acquire) {
            return;
        }
        let connection_state = state.clone();
        let connection = tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, connection_state).await {
                debug!(error = %error, "tool proxy connection closed");
            }
        });
        state.connections.lock().await.push(connection);
    }
}

async fn handle_connection(mut stream: UnixStream, state: Arc<ProxyState>) -> anyhow::Result<()> {
    if state.stopping.load(Ordering::Acquire) {
        return Ok(());
    }
    let first_frame = tokio::select! {
        result = read_frame(&mut stream) => result,
        _ = state.shutdown.notified() => return Ok(()),
    };
    let (kind, payload) = match first_frame {
        Ok(frame) => frame,
        Err(error) => {
            let _ = send_failure(&mut stream, format!("invalid OPEN frame: {error}")).await;
            return Err(error);
        }
    };
    if kind != OPEN {
        return send_failure(&mut stream, "first frame must be OPEN".into()).await;
    }
    let request = match parse_open(&payload) {
        Ok(request) => request,
        Err(error) => {
            return send_failure(&mut stream, format!("invalid OPEN frame: {error}")).await
        }
    };
    let Some(tool) = state.tools.get(&request.command).cloned() else {
        return send_failure(
            &mut stream,
            format!("unknown tool command: {}", request.command),
        )
        .await;
    };

    let environment = match tool.env_path.as_deref() {
        Some(env_path) => match load_dotenv(env_path) {
            Ok(environment) => environment,
            Err(error) => {
                return send_failure(
                    &mut stream,
                    format!("failed to load tool environment: {error}"),
                )
                .await;
            }
        },
        None => HashMap::new(),
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
            return send_failure(
                &mut stream,
                format!("failed to start tool {}: {error}", request.command),
            )
            .await;
        }
    };
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("tool stdin is unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("tool stdout is unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("tool stderr is unavailable"))?;

    if state.stopping.load(Ordering::Acquire) {
        kill_process_group(&mut child).await;
        let _ = child.wait().await;
        return Ok(());
    }

    let (reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));
    let mut stdin_task = tokio::spawn(forward_stdin(reader, stdin));
    let mut stdout_task = tokio::spawn(forward_output(stdout, writer.clone(), STDOUT));
    let mut stderr_task = tokio::spawn(forward_output(stderr, writer.clone(), STDERR));
    let mut stdin_done = false;
    let mut stdout_done = false;
    let mut stderr_done = false;

    let status = loop {
        if let Some(status) = child.try_wait().context("inspect tool status")? {
            break status;
        }

        tokio::select! {
            result = &mut stdin_task, if !stdin_done => {
                stdin_done = true;
                let result = result.context("join stdin forwarding")?;
                if let Err(error) = result {
                    debug!(error = %error, "stdin forwarding failed; terminating tool");
                    let message = format!("stdin protocol failure: {error}");
                    let mut writer = writer.lock().await;
                    let _ = write_frame(&mut *writer, ERROR, message.as_bytes()).await;
                    drop(writer);
                    kill_process_group(&mut child).await;
                    break child.wait().await.context("wait for terminated tool")?;
                }
            }
            result = &mut stdout_task, if !stdout_done => {
                stdout_done = true;
                let result = result.context("join stdout forwarding")?;
                if let Err(error) = result {
                    debug!(error = %error, "stdout forwarding failed; terminating tool");
                    kill_process_group(&mut child).await;
                    break child.wait().await.context("wait for terminated tool")?;
                }
            }
            result = &mut stderr_task, if !stderr_done => {
                stderr_done = true;
                let result = result.context("join stderr forwarding")?;
                if let Err(error) = result {
                    debug!(error = %error, "stderr forwarding failed; terminating tool");
                    kill_process_group(&mut child).await;
                    break child.wait().await.context("wait for terminated tool")?;
                }
            }
            _ = state.shutdown.notified() => {
                kill_process_group(&mut child).await;
                break child.wait().await.context("wait for shutdown tool")?;
            }
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
    };

    if !stdin_done {
        stdin_task.abort();
        let _ = stdin_task.await;
    }
    if !stdout_done {
        stdout_task.await.context("join stdout forwarding")??;
    }
    if !stderr_done {
        stderr_task.await.context("join stderr forwarding")??;
    }

    let code = exit_code(status);
    let mut writer = writer.lock().await;
    write_frame(&mut *writer, EXIT, &code.to_be_bytes()).await
}

async fn send_failure(stream: &mut UnixStream, message: String) -> anyhow::Result<()> {
    write_frame(stream, ERROR, message.as_bytes()).await?;
    write_frame(stream, EXIT, &TOOL_START_FAILURE.to_be_bytes()).await
}

async fn forward_stdin(
    mut reader: tokio::net::unix::OwnedReadHalf,
    mut stdin: tokio::process::ChildStdin,
) -> anyhow::Result<()> {
    loop {
        let (kind, payload) = read_frame(&mut reader).await?;
        match kind {
            STDIN => stdin.write_all(&payload).await?,
            STDIN_EOF if payload.is_empty() => {
                stdin.shutdown().await?;
                return Ok(());
            }
            _ => bail!("unexpected frame on stdin channel: {kind:#x}"),
        }
    }
}

async fn forward_output<R>(
    mut output: R,
    writer: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    kind: u8,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; 32 * 1024];
    loop {
        let length = output.read(&mut buffer).await?;
        if length == 0 {
            return Ok(());
        }
        let mut writer = writer.lock().await;
        write_frame(&mut *writer, kind, &buffer[..length]).await?;
    }
}

struct Request {
    command: String,
    args: Vec<OsString>,
}

fn parse_open(payload: &[u8]) -> anyhow::Result<Request> {
    let mut input = FrameReader::new(payload);
    let command = String::from_utf8(input.field()?).context("tool command is not valid UTF-8")?;
    if command.is_empty() {
        bail!("tool command must not be empty");
    }
    let argc = input.u32()?;
    if argc > MAX_ARGUMENTS {
        bail!("argument count exceeds {MAX_ARGUMENTS}");
    }
    let mut args = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        args.push(OsString::from_vec(input.field()?));
    }
    input.finish()?;
    Ok(Request { command, args })
}

struct FrameReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> FrameReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn u32(&mut self) -> anyhow::Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn field(&mut self) -> anyhow::Result<Vec<u8>> {
        let length = self.u32()?;
        if length > MAX_FIELD_BYTES {
            bail!("field length exceeds {MAX_FIELD_BYTES}");
        }
        Ok(self.take(length as usize)?.to_vec())
    }

    fn take(&mut self, length: usize) -> anyhow::Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| anyhow!("frame length overflow"))?;
        if end > self.bytes.len() {
            bail!("truncated frame payload");
        }
        let value = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(value)
    }

    fn finish(self) -> anyhow::Result<()> {
        if self.offset != self.bytes.len() {
            bail!("unexpected trailing bytes in OPEN frame");
        }
        Ok(())
    }
}

async fn read_frame<R>(reader: &mut R) -> anyhow::Result<(u8, Vec<u8>)>
where
    R: AsyncRead + Unpin,
{
    let kind = reader.read_u8().await?;
    let length = reader.read_u32().await?;
    if length > MAX_FRAME_BYTES {
        bail!("frame length exceeds {MAX_FRAME_BYTES}");
    }
    let mut payload = vec![0_u8; length as usize];
    reader.read_exact(&mut payload).await?;
    Ok((kind, payload))
}

async fn write_frame<W>(writer: &mut W, kind: u8, payload: &[u8]) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    if payload.len() > MAX_FRAME_BYTES as usize {
        bail!("frame length exceeds {MAX_FRAME_BYTES}");
    }
    writer.write_u8(kind).await?;
    writer.write_u32(payload.len() as u32).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
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
            bail!("line {} has an invalid environment key", line_number + 1);
        }
        let mut value = raw_value.trim().to_owned();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = value[1..value.len() - 1].to_owned();
        }
        if value.contains('\0') {
            bail!("line {} contains NUL", line_number + 1);
        }
        values.insert(key.to_owned(), value);
    }
    Ok(values)
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(TOOL_START_FAILURE)
}

async fn kill_process_group(child: &mut Child) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::fs::PermissionsExt, time::Duration};
    use tempfile::tempdir;

    fn config(root: &Path) -> SandboxConfig {
        let mut config: SandboxConfig = toml::from_str("rootfs_dir = \".\"").unwrap();
        config.rootfs_dir = root.to_path_buf();
        config.managed_bundle_dir = root.join("bundle");
        config
    }

    async fn open(stream: &mut UnixStream, command: &str, args: &[&[u8]]) {
        let mut payload = Vec::new();
        payload.extend_from_slice(&(command.len() as u32).to_be_bytes());
        payload.extend_from_slice(command.as_bytes());
        payload.extend_from_slice(&(args.len() as u32).to_be_bytes());
        for arg in args {
            payload.extend_from_slice(&(arg.len() as u32).to_be_bytes());
            payload.extend_from_slice(arg);
        }
        write_frame(stream, OPEN, &payload).await.unwrap();
    }

    #[tokio::test]
    async fn proxy_executes_tool_and_routes_all_streams() {
        let temp = tempdir().unwrap();
        let script = temp.path().join("tool.sh");
        fs::write(
            &script,
            "#!/bin/sh\ncat\nprintf 'err:%s' \"$TOOL_SECRET\" >&2\nexit 7\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let env = temp.path().join("tool.env");
        fs::write(&env, "TOOL_SECRET=secret\n").unwrap();
        let cfg = config(temp.path());
        let configured_tools = HashMap::from([(
            "something-cli".into(),
            ToolConfig {
                path: script,
                env: Some(env),
            },
        )]);
        let proxy = ToolProxy::start(&cfg, &configured_tools)
            .await
            .unwrap()
            .unwrap();

        let mut stream = UnixStream::connect(&proxy.socket_path).await.unwrap();
        open(&mut stream, "something-cli", &[]).await;
        write_frame(&mut stream, STDIN, b"hello\0binary\n")
            .await
            .unwrap();
        write_frame(&mut stream, STDIN_EOF, &[]).await.unwrap();

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = loop {
            let (kind, payload) =
                tokio::time::timeout(Duration::from_secs(2), read_frame(&mut stream))
                    .await
                    .unwrap()
                    .unwrap();
            match kind {
                STDOUT => stdout.extend(payload),
                STDERR => stderr.extend(payload),
                EXIT => {
                    break i32::from_be_bytes(payload.try_into().unwrap());
                }
                ERROR => panic!(
                    "unexpected proxy error: {}",
                    String::from_utf8_lossy(&payload)
                ),
                _ => panic!("unexpected response frame {kind:#x}"),
            }
        };
        assert_eq!(stdout, b"hello\0binary\n");
        assert_eq!(stderr, b"err:secret");
        assert_eq!(code, 7);
        proxy.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn proxy_does_not_inject_environment_without_env_file() {
        let temp = tempdir().unwrap();
        let script = temp.path().join("tool.sh");
        fs::write(&script, "#!/bin/sh\nprintf '%s' \"${TOOL_SECRET-unset}\"\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let cfg = config(temp.path());
        let configured_tools = HashMap::from([(
            "something-cli".into(),
            ToolConfig {
                path: script,
                env: None,
            },
        )]);
        let proxy = ToolProxy::start(&cfg, &configured_tools)
            .await
            .unwrap()
            .unwrap();

        let mut stream = UnixStream::connect(&proxy.socket_path).await.unwrap();
        open(&mut stream, "something-cli", &[]).await;
        write_frame(&mut stream, STDIN_EOF, &[]).await.unwrap();

        let mut stdout = Vec::new();
        loop {
            let (kind, payload) = read_frame(&mut stream).await.unwrap();
            match kind {
                STDOUT => stdout.extend(payload),
                EXIT => break,
                ERROR => panic!(
                    "unexpected proxy error: {}",
                    String::from_utf8_lossy(&payload)
                ),
                _ => {}
            }
        }
        assert_eq!(stdout, b"unset");
        proxy.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn disconnecting_socket_terminates_the_tool_group() {
        let temp = tempdir().unwrap();
        let marker = temp.path().join("marker");
        let script = temp.path().join("long-tool.sh");
        fs::write(
            &script,
            format!("#!/bin/sh\nsleep 30\nprintf done > {}\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let env = temp.path().join("tool.env");
        fs::write(&env, "").unwrap();
        let cfg = config(temp.path());
        let configured_tools = HashMap::from([(
            "long-tool".into(),
            ToolConfig {
                path: script,
                env: Some(env),
            },
        )]);
        let proxy = ToolProxy::start(&cfg, &configured_tools)
            .await
            .unwrap()
            .unwrap();
        let mut stream = UnixStream::connect(&proxy.socket_path).await.unwrap();
        open(&mut stream, "long-tool", &[]).await;
        drop(stream);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!marker.exists());
        proxy.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_terminates_active_tool_connections() {
        let temp = tempdir().unwrap();
        let marker = temp.path().join("marker");
        let script = temp.path().join("shutdown-tool.sh");
        fs::write(
            &script,
            format!("#!/bin/sh\nsleep 30\nprintf done > {}\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let env = temp.path().join("tool.env");
        fs::write(&env, "").unwrap();
        let cfg = config(temp.path());
        let configured_tools = HashMap::from([(
            "shutdown-tool".into(),
            ToolConfig {
                path: script,
                env: Some(env),
            },
        )]);
        let proxy = ToolProxy::start(&cfg, &configured_tools)
            .await
            .unwrap()
            .unwrap();
        let mut stream = UnixStream::connect(&proxy.socket_path).await.unwrap();
        open(&mut stream, "shutdown-tool", &[]).await;
        proxy.shutdown().await.unwrap();
        drop(stream);
        assert!(!marker.exists());
    }

    #[test]
    fn dotenv_is_loaded_again_after_changes() {
        let temp = tempdir().unwrap();
        let env = temp.path().join("tool.env");
        fs::write(&env, "SECRET=one\n").unwrap();
        assert_eq!(load_dotenv(&env).unwrap()["SECRET"], "one");
        fs::write(&env, "SECRET=two\n").unwrap();
        assert_eq!(load_dotenv(&env).unwrap()["SECRET"], "two");
    }

    #[test]
    fn open_payload_rejects_trailing_bytes() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&1_u32.to_be_bytes());
        payload.extend_from_slice(b"x");
        payload.extend_from_slice(&0_u32.to_be_bytes());
        payload.push(1);
        assert!(parse_open(&payload).is_err());
    }
}
