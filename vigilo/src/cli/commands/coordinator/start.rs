//! Long-running coordinator mode.
//!
//! Used by `vigilo coordinator start` to run coordinator cycles on a fixed tick
//! until process shutdown. The same coordinator id is reused across cycles, and
//! all durable leasing/finalization behavior is delegated to the shared cycle
//! implementation.

use super::*;

/// Starts the long-running coordinator loop.
pub(super) async fn exec(context: Context, config: CoordinatorRuntimeConfig) -> anyhow::Result<()> {
    // One logical coordinator id is reused across loop iterations.
    let coordinator_id = Uuid::now_v7();
    ServiceRunner::new("coordinator")
        .tick_interval(Duration::from_secs(config.tick_seconds))
        .run_loop(move || {
            let context = context.clone();
            let config = config.clone();
            async move { run_coordinator_cycle(context, coordinator_id, &config).await }
        })
        .await
}
