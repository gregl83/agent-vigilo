use std::{
    future::Future,
    time::Duration,
};

use tokio_util::sync::CancellationToken;
use tracing::{
    error,
    info,
    warn,
};

pub async fn run_service<F, Fut>(service_name: &'static str, run: F) -> anyhow::Result<()>
where
    F: FnOnce(CancellationToken) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let shutdown = CancellationToken::new();

    info!(service = service_name, "service started");

    tokio::select! {
        result = run(shutdown.clone()) => {
            match result {
                Ok(()) => {
                    warn!(service = service_name, "service exited normally");
                    anyhow::bail!("{service_name} exited unexpectedly");
                }
                Err(err) => {
                    error!(service = service_name, error = ?err, "service failed");
                    Err(err)
                }
            }
        }

        _ = shutdown_signal() => {
            info!(service = service_name, "shutdown signal received");
            shutdown.cancel();

            // optional small grace period for tasks observing cancellation.
            tokio::time::sleep(Duration::from_millis(100)).await;

            info!(service = service_name, "service shutdown complete");
            Ok(())
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
