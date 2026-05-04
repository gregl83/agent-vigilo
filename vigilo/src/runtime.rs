use std::{
    future::Future,
    pin::Pin,
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

/// Fluent runtime helper for long-running services.
///
/// Loop-style service usage (no explicit shutdown hook):
///
/// ```ignore
/// use std::time::Duration;
///
/// ServiceRunner::new("worker")
///     .shutdown_timeout(Duration::from_secs(55))
///     .run_loop(|| async {
///         // Process one unit of work per tick.
///         process_one_job().await
///     })
///     .await
/// ```
///
/// Task-style service usage (with optional shutdown hook):
///
/// ```ignore
/// use std::time::Duration;
/// use tokio_util::sync::CancellationToken;
///
/// ServiceRunner::new("worker")
///     .shutdown_timeout(Duration::from_secs(55))
///     .on_shutdown(|| async {
///         // Best-effort cleanup after cancellation is requested.
///         release_current_job().await?;
///         flush_metrics().await?;
///         Ok(())
///     })
///     .run(|shutdown: CancellationToken| async move {
///         while !shutdown.is_cancelled() {
///             process_one_job().await?;
///         }
///
///         Ok(())
///     })
///     .await
/// ```
#[derive(Debug)]
pub struct ServiceRunner<ShutdownHook = NoShutdownHook> {
    service_name: &'static str,
    shutdown_timeout: Duration,
    shutdown_hook: ShutdownHook,
}

/// Marker hook used when no explicit shutdown callback is configured.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoShutdownHook;

pub(crate) trait ShutdownCallback {
    fn call(self) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;
}

impl ShutdownCallback for NoShutdownHook {
    fn call(self) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
        Box::pin(async { Ok(()) })
    }
}

impl<F, Fut> ShutdownCallback for F
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    fn call(self) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
        Box::pin(self())
    }
}

impl ServiceRunner<NoShutdownHook> {
    /// Creates a runner with a default shutdown timeout of 30 seconds.
    pub fn new(service_name: &'static str) -> Self {
        Self {
            service_name,
            shutdown_timeout: Duration::from_secs(30),
            shutdown_hook: NoShutdownHook,
        }
    }

    /// Registers an optional hook that runs after cancellation is requested.
    ///
    /// The hook runs only after the runtime receives a shutdown signal and
    /// cancels the service token.
    ///
    /// ```ignore
    /// ServiceRunner::new("coordinator")
    ///     .on_shutdown(|| async {
    ///         persist_checkpoint().await?;
    ///         Ok(())
    ///     });
    /// ```
    pub fn on_shutdown<Hook, HookFut>(self, shutdown_hook: Hook) -> ServiceRunner<Hook>
    where
        Hook: FnOnce() -> HookFut,
        HookFut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        ServiceRunner {
            service_name: self.service_name,
            shutdown_timeout: self.shutdown_timeout,
            shutdown_hook,
        }
    }
}

impl<ShutdownHook> ServiceRunner<ShutdownHook>
where
    ShutdownHook: ShutdownCallback,
{
    /// Sets the maximum time to wait for cooperative shutdown before aborting.
    pub fn shutdown_timeout(mut self, shutdown_timeout: Duration) -> Self {
        self.shutdown_timeout = shutdown_timeout;
        self
    }

    /// Runs a service loop until process shutdown is requested.
    ///
    /// ```ignore
    /// ServiceRunner::new("coordinator")
    ///     .run_loop(|| async {
    ///         run_one_coordinator_cycle().await
    ///     })
    ///     .await?;
    ///
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub async fn run_loop<Tick, TickFut>(self, mut tick: Tick) -> anyhow::Result<()>
    where
        Tick: FnMut() -> TickFut + Send + 'static,
        TickFut: Future<Output = anyhow::Result<()>> + Send,
    {
        let service_name = self.service_name;

        self.run(move |shutdown| async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        info!(service = service_name, "service stopping");
                        return Ok(());
                    }

                    result = tick() => {
                        result?;
                    }
                }
            }
        })
        .await
    }

    /// Runs a long-lived service task and coordinates graceful shutdown.
    ///
    /// ```ignore
    /// use tokio_util::sync::CancellationToken;
    ///
    /// ServiceRunner::new("worker")
    ///     .run(|shutdown: CancellationToken| async move {
    ///         while !shutdown.is_cancelled() {
    ///             process_one_job().await?;
    ///         }
    ///
    ///         Ok(())
    ///     })
    ///     .await?;
    ///
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub async fn run<Run, RunFut>(self, run: Run) -> anyhow::Result<()>
    where
        Run: FnOnce(CancellationToken) -> RunFut,
        RunFut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let shutdown = CancellationToken::new();

        info!(service = self.service_name, "service started");

        let mut service_task = tokio::spawn(run(shutdown.clone()));

        tokio::select! {
            join_result = &mut service_task => {
                let result = join_result
                    .with_context(|| format!("{} task panicked or was cancelled", self.service_name))?;

                match result {
                    Ok(()) => {
                        warn!(
                            service = self.service_name,
                            "service exited normally without shutdown signal"
                        );

                        anyhow::bail!("{} exited unexpectedly", self.service_name);
                    }

                    Err(err) => {
                        error!(
                            service = self.service_name,
                            error = ?err,
                            "service failed"
                        );

                        Err(err)
                    }
                }
            }

            _ = shutdown_signal() => {
                info!(service = self.service_name, "shutdown signal received");

                shutdown.cancel();
                // run caller-provided cleanup after cancellation is requested so
                // dependencies (queues, db pools, telemetry) can drain/flush first
                self.shutdown_hook.call().await?;

                info!(
                    service = self.service_name,
                    timeout_secs = self.shutdown_timeout.as_secs(),
                    "waiting for service task to stop"
                );

                wait_for_service_stop(self.service_name, &mut service_task, self.shutdown_timeout).await?;

                info!(service = self.service_name, "service shutdown complete");

                Ok(())
            }
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
