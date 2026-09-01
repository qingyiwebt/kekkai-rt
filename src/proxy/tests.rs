use super::{
    protocol::{read_frame, write_frame, FrameKind},
    server::ToolProxy,
};
use crate::config::{SandboxConfig, ToolConfig};
use std::{
    collections::HashMap, fs, os::unix::fs::PermissionsExt, path::Path, sync::OnceLock,
    time::Duration,
};
use tempfile::{Builder, TempDir};
use tokio::net::UnixStream;

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
    write_frame(stream, FrameKind::Open, &payload)
        .await
        .unwrap();
}
fn configured_tool(
    name: &str,
    script: std::path::PathBuf,
    env: Option<std::path::PathBuf>,
) -> HashMap<String, ToolConfig> {
    HashMap::from([(name.into(), ToolConfig { path: script, env })])
}
fn socket_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}
fn proxy_tempdir() -> TempDir {
    Builder::new()
        .prefix("proxy-test-")
        .tempdir_in("/tmp")
        .unwrap()
}
async fn start_proxy(
    cfg: &SandboxConfig,
    tools: &HashMap<String, ToolConfig>,
) -> Option<ToolProxy> {
    match ToolProxy::start(cfg, tools).await {
        Ok(Some(proxy)) => Some(proxy),
        Ok(None) => unreachable!("test config always contains one tool"),
        Err(error)
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
            }) =>
        {
            eprintln!("skipping Unix socket integration test: {error}");
            None
        }
        Err(error) => panic!("start tool proxy: {error:#}"),
    }
}

#[tokio::test]
async fn proxy_executes_tool_and_routes_all_streams() {
    let _guard = socket_test_lock().lock().await;
    let temp = proxy_tempdir();
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
    let tools = configured_tool("something-cli", script, Some(env));
    let Some(proxy) = start_proxy(&cfg, &tools).await else {
        return;
    };
    let mut stream = UnixStream::connect(proxy.socket_path()).await.unwrap();
    open(&mut stream, "something-cli", &[]).await;
    write_frame(&mut stream, FrameKind::Stdin, b"hello\0binary\n")
        .await
        .unwrap();
    write_frame(&mut stream, FrameKind::StdinEof, &[])
        .await
        .unwrap();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = loop {
        let frame = tokio::time::timeout(Duration::from_secs(2), read_frame(&mut stream))
            .await
            .unwrap()
            .unwrap();
        match frame.kind {
            FrameKind::Stdout => stdout.extend(frame.payload),
            FrameKind::Stderr => stderr.extend(frame.payload),
            FrameKind::Exit => break i32::from_be_bytes(frame.payload.try_into().unwrap()),
            FrameKind::Error => panic!(
                "unexpected proxy error: {}",
                String::from_utf8_lossy(&frame.payload)
            ),
            kind => panic!("unexpected response frame {kind:?}"),
        }
    };
    assert_eq!(stdout, b"hello\0binary\n");
    assert_eq!(stderr, b"err:secret");
    assert_eq!(code, 7);
    proxy.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_or_unknown_request_returns_error_then_start_failure() {
    let _guard = socket_test_lock().lock().await;
    let temp = proxy_tempdir();
    let script = temp.path().join("tool.sh");
    fs::write(&script, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = config(temp.path());
    let tools = configured_tool("tool", script, None);
    let Some(proxy) = start_proxy(&cfg, &tools).await else {
        return;
    };
    let mut stream = UnixStream::connect(proxy.socket_path()).await.unwrap();
    open(&mut stream, "missing", &[]).await;
    assert_eq!(
        read_frame(&mut stream).await.unwrap().kind,
        FrameKind::Error
    );
    let exit = read_frame(&mut stream).await.unwrap();
    assert_eq!(exit.kind, FrameKind::Exit);
    assert_eq!(i32::from_be_bytes(exit.payload.try_into().unwrap()), 127);
    proxy.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_terminates_active_tool_connections() {
    let _guard = socket_test_lock().lock().await;
    let temp = proxy_tempdir();
    let marker = temp.path().join("marker");
    let script = temp.path().join("long-tool.sh");
    fs::write(
        &script,
        format!("#!/bin/sh\nsleep 30\nprintf done > {}\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = config(temp.path());
    let tools = configured_tool("long-tool", script, None);
    let Some(proxy) = start_proxy(&cfg, &tools).await else {
        return;
    };
    let mut stream = UnixStream::connect(proxy.socket_path()).await.unwrap();
    open(&mut stream, "long-tool", &[]).await;
    proxy.shutdown().await.unwrap();
    drop(stream);
    assert!(!marker.exists());
}
