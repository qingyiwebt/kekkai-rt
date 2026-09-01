use anyhow::Context;
use kekkai_rt::{api, config::Config, runtime::Sandbox};
use std::{path::Path, sync::Arc};
use tracing::info;

pub async fn run_server(config_path: &Path) -> anyhow::Result<()> {
    let config = Config::load(config_path).context("load configuration")?;
    let sandbox = Arc::new(
        Sandbox::start(
            &config.sandbox,
            &config.features,
            &config.mounts,
            &config.tools,
        )
        .await
        .context("start sandbox")?,
    );
    let state = api::AppState::new(config.clone(), sandbox.clone());
    let app = api::router(state.clone());
    let listener = match tokio::net::TcpListener::bind(config.api.listen_addr).await {
        Ok(listener) => listener,
        Err(error) => {
            if let Err(cleanup_error) = sandbox.shutdown().await {
                return Err(anyhow::anyhow!(
                    "bind API listener: {error}; sandbox shutdown failed: {cleanup_error}"
                ));
            }
            return Err(error.into());
        }
    };
    info!(address = %listener.local_addr()?, backend = %config.sandbox.backend, "kekkai-rt listening");
    let server_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    info!("HTTP server stopped; shutting down sandbox");
    let shutdown_result = state.shutdown().await;
    match (server_result, shutdown_result) {
        (Err(server_error), Err(shutdown_error)) => Err(anyhow::anyhow!(
            "server failed: {server_error}; sandbox shutdown failed: {shutdown_error}"
        )),
        (Err(error), Ok(())) => Err(error.into()),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => info!("received Ctrl-C"),
        _ = terminate.recv() => info!("received SIGTERM"),
    }
}
