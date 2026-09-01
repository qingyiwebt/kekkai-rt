use super::{
    environment::load_dotenv,
    protocol::{parse_open, read_frame, write_frame, FrameKind, TOOL_START_FAILURE},
    server::ProxyState,
};
use anyhow::{anyhow, bail, Context};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    process::{Child, Command},
    sync::Mutex,
};
use tracing::debug;

pub async fn handle_connection(
    mut stream: UnixStream,
    state: Arc<ProxyState>,
) -> anyhow::Result<()> {
    let first_frame = tokio::select! { result = read_frame(&mut stream) => result, _ = state.shutdown.notified() => return Ok(()) };
    let frame = match first_frame {
        Ok(frame) => frame,
        Err(error) => {
            let _ = send_failure(&mut stream, format!("invalid OPEN frame: {error}")).await;
            return Err(error);
        }
    };
    if frame.kind != FrameKind::Open {
        return send_failure(&mut stream, "first frame must be OPEN".into()).await;
    }
    let request = match parse_open(&frame.payload) {
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
    let environment = match tool.env_path.as_deref().map(load_dotenv).transpose() {
        Ok(Some(environment)) => environment,
        Ok(None) => Default::default(),
        Err(error) => {
            return send_failure(
                &mut stream,
                format!("failed to load tool environment: {error}"),
            )
            .await
        }
    };
    debug!(tool = %request.command, "starting proxied tool");
    let mut command = Command::new(&tool.path);
    command
        .env_clear()
        .args(request.args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .envs(environment);
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
            .await
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
    if state.is_stopping() {
        kill_process_group(&mut child).await;
        let _ = child.wait().await;
        return Ok(());
    }
    let (reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));
    let mut stdin_task = tokio::spawn(forward_stdin(reader, stdin));
    let mut stdout_task = tokio::spawn(forward_output(stdout, writer.clone(), FrameKind::Stdout));
    let mut stderr_task = tokio::spawn(forward_output(stderr, writer.clone(), FrameKind::Stderr));
    let mut stdin_done = false;
    let mut stdout_done = false;
    let mut stderr_done = false;
    let status = loop {
        if let Some(status) = child.try_wait().context("inspect tool status")? {
            break status;
        }
        tokio::select! {
            result = &mut stdin_task, if !stdin_done => { stdin_done = true; if let Err(error) = result.context("join stdin forwarding")? { debug!(tool = %request.command, error = %error, "stdin forwarding failed; terminating tool"); let mut writer = writer.lock().await; let _ = write_frame(&mut *writer, FrameKind::Error, format!("stdin protocol failure: {error}").as_bytes()).await; drop(writer); terminate_and_wait(&mut child).await?; break child.wait().await.context("wait for terminated tool")?; } }
            result = &mut stdout_task, if !stdout_done => { stdout_done = true; if let Err(error) = result.context("join stdout forwarding")? { debug!(tool = %request.command, error = %error, "stdout forwarding failed; terminating tool"); terminate_and_wait(&mut child).await?; break child.wait().await.context("wait for terminated tool")?; } }
            result = &mut stderr_task, if !stderr_done => { stderr_done = true; if let Err(error) = result.context("join stderr forwarding")? { debug!(tool = %request.command, error = %error, "stderr forwarding failed; terminating tool"); terminate_and_wait(&mut child).await?; break child.wait().await.context("wait for terminated tool")?; } }
            _ = state.shutdown.notified() => { debug!(tool = %request.command, "stopping proxied tool during proxy shutdown"); terminate_and_wait(&mut child).await?; break child.wait().await.context("wait for shutdown tool")?; }
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
    debug!(tool = %request.command, exit_code = code, "proxied tool exited");
    let mut writer = writer.lock().await;
    write_frame(&mut *writer, FrameKind::Exit, &code.to_be_bytes()).await
}

async fn send_failure(stream: &mut UnixStream, message: String) -> anyhow::Result<()> {
    write_frame(stream, FrameKind::Error, message.as_bytes()).await?;
    write_frame(stream, FrameKind::Exit, &TOOL_START_FAILURE.to_be_bytes()).await
}
async fn forward_stdin(
    mut reader: tokio::net::unix::OwnedReadHalf,
    mut stdin: tokio::process::ChildStdin,
) -> anyhow::Result<()> {
    loop {
        let frame = read_frame(&mut reader).await?;
        match frame.kind {
            FrameKind::Stdin => stdin.write_all(&frame.payload).await?,
            FrameKind::StdinEof if frame.payload.is_empty() => {
                stdin.shutdown().await?;
                return Ok(());
            }
            kind => bail!("unexpected frame on stdin channel: {kind:?}"),
        }
    }
}
async fn forward_output<R: AsyncRead + Unpin>(
    mut output: R,
    writer: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    kind: FrameKind,
) -> anyhow::Result<()> {
    let mut buffer = vec![0; 32 * 1024];
    loop {
        let length = output.read(&mut buffer).await?;
        if length == 0 {
            return Ok(());
        }
        let mut writer = writer.lock().await;
        write_frame(&mut *writer, kind, &buffer[..length]).await?;
    }
}
fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(TOOL_START_FAILURE)
}
async fn terminate_and_wait(child: &mut Child) -> anyhow::Result<()> {
    kill_process_group(child).await;
    Ok(())
}
async fn kill_process_group(child: &mut Child) {
    let Some(pid) = child.id() else {
        return;
    };
    if unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) } != 0 {
        let _ = child.kill().await;
    }
}
