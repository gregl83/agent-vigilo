use std::{
    future::Future,
    time::Duration,
};

use anyhow::Context;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{
    error,
    info,
    warn,
};


/*
todo - refactor to use builder pattern

ServiceRunner::new("worker")
    .shutdown_timeout(Duration::from_secs(55))
    .on_shutdown(|| async {
        release_current_job().await?;
        flush_metrics().await?;
        Ok(())
    })
    .run_loop(process_one_job)
    .await
 */



pub async fn run_loop_service<Tick, TickFut, Cleanup, CleanupFut>(
    service_name: &'static str,
    shutdown: CancellationToken,
    mut tick: Tick,
    cleanup: Cleanup,
) -> anyhow::Result<()>
where
    Tick: FnMut() -> TickFut,
    TickFut: Future<Output = anyhow::Result<()>>,
    Cleanup: FnOnce() -> CleanupFut,
    CleanupFut: Future<Output = anyhow::Result<()>>,
{
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                info!(service = service_name, "service stopping");
                cleanup().await?;
                return Ok(());
            }

            result = tick() => {
                result?;
            }
        }
    }
}

/// Runs a long-lived service task and coordinates graceful shutdown.
///
/// The `run` function performs cooperative shutdown when it observes the
/// `CancellationToken`, allowing it to complete in-flight work and release
/// resources in an ordered manner.
///
/// However, this relies on the service loop making progress. In cases where the
/// loop is slow, blocked, or unable to reach a cancellation point within the
/// shutdown deadline, `shutdown_hook` provides an out-of-band, best-effort path
/// for critical state transitions required for safe shutdown.
///
/// The `shutdown_hook` should be fast, idempotent, and limited to external state
/// changes (e.g., marking the instance as draining or releasing leases).
///
/// Returns an error when:
/// - Service task returns an error
/// - Service task panics or is canceled
/// - Service exits normally before any shutdown signal
/// - Graceful stop exceeds `shutdown_timeout`
pub async fn run_service<Run, RunFut, Shutdown, ShutdownFut>(
    service_name: &'static str,
    shutdown_timeout: Duration,
    run: Run,
    shutdown_hook: Shutdown,
) -> anyhow::Result<()>
where
    Run: FnOnce(CancellationToken) -> RunFut,
    RunFut: Future<Output = anyhow::Result<()>> + Send + 'static,
    Shutdown: FnOnce() -> ShutdownFut,
    ShutdownFut: Future<Output = anyhow::Result<()>>,
{
    let shutdown = CancellationToken::new();

    info!(service = service_name, "service started");

    let mut service_task = tokio::spawn(run(shutdown.clone()));

    tokio::select! {
        join_result = &mut service_task => {
            let result = join_result
                .with_context(|| format!("{service_name} task panicked or was cancelled"))?;

            match result {
                Ok(()) => {
                    warn!(
                        service = service_name,
                        "service exited normally without shutdown signal"
                    );

                    anyhow::bail!("{service_name} exited unexpectedly");
                }

                Err(err) => {
                    error!(
                        service = service_name,
                        error = ?err,
                        "service failed"
                    );

                    Err(err)
                }
            }
        }

        _ = shutdown_signal() => {
            info!(service = service_name, "shutdown signal received");

            shutdown.cancel();
            // run caller-provided cleanup after cancellation is requested so
            // dependencies (queues, db pools, telemetry) can drain/flush first
            shutdown_hook().await?;

            info!(
                service = service_name,
                timeout_secs = shutdown_timeout.as_secs(),
                "waiting for service task to stop"
            );

            wait_for_service_stop(service_name, &mut service_task, shutdown_timeout).await?;

            info!(service = service_name, "service shutdown complete");

            Ok(())
        }
    }
}

/// Waits for the service task to stop and aborts it if timeout is reached.
async fn wait_for_service_stop(
    service_name: &'static str,
    service_task: &mut JoinHandle<anyhow::Result<()>>,
    timeout: Duration,
) -> anyhow::Result<()> {
    // wait only up to `timeout` for a cooperative shutdown. if the task keeps
    // running, force-stop it with `abort` so process shutdown can continue
    match tokio::time::timeout(timeout, &mut *service_task).await {
        Ok(join_result) => {
            join_result
                .with_context(|| format!("{service_name} task panicked or was cancelled"))??;

            Ok(())
        }

        Err(_) => {
            service_task.abort();
            anyhow::bail!("{service_name} did not stop within {:?}", timeout);
        }
    }
}

/// Resolves when a process termination signal is observed.
///
/// On Unix this waits for Ctrl-C or SIGTERM. On non-Unix platforms this waits
/// only for Ctrl-C.
async fn shutdown_signal() {
    // ctrl-C is always supported and is the cross-platform shutdown trigger
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    // On Unix, also listen for SIGTERM (common in containers/process managers).
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    // keep select! shape consistent when SIGTERM is unavailable
    let terminate = std::future::pending::<()>();

    // resolve once either shutdown trigger fires
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
