use super::*;

/// Starts the long-running coordinator loop.
pub(super) async fn exec(context: Context) -> anyhow::Result<()> {
    // One logical coordinator id is reused across loop iterations.
    let coordinator_id = Uuid::now_v7();
    ServiceRunner::new("coordinator")
        .tick_interval(Duration::from_secs(COORDINATOR_TICK_SECONDS))
        .run_loop(move || {
            let context = context.clone();
            async move { run_coordinator_cycle(context, coordinator_id).await }
        })
        .await
}
