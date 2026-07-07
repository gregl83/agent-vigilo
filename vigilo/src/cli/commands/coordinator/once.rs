//! Single-pass coordinator mode.
//!
//! Used by `vigilo coordinator once` to execute exactly one orchestration cycle
//! with a fresh coordinator id. This module should only bridge CLI mode
//! selection to the shared coordinator cycle; ordering and lease rules belong
//! in the parent coordinator module and database workflows.

use super::*;

/// Runs a single coordinator cycle with a fresh coordinator id.
///
/// Useful for cron-like orchestration or local debugging.
pub(super) async fn exec(context: Context, config: CoordinatorRuntimeConfig) -> anyhow::Result<()> {
    let coordinator_id = Uuid::now_v7();
    run_coordinator_cycle(context, coordinator_id, &config).await
}
