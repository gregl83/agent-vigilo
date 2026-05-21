use super::*;

/// Runs a single coordinator cycle with a fresh coordinator id.
///
/// Useful for cron-like orchestration or local debugging.
pub(super) async fn exec(context: Context, config: CoordinatorRuntimeConfig) -> anyhow::Result<()> {
    let coordinator_id = Uuid::now_v7();
    run_coordinator_cycle(context, coordinator_id, &config).await
}
